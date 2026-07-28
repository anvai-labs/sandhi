//! Canonical chat v1 ↔ Anthropic Messages codec.

use crate::typed::{provider_request, ChatEventStream, ChatProvider};
use crate::{ByteStream, ParsedUsage, Provider, ProviderError};
use async_trait::async_trait;
use sandhi_core::{
    AssistantOutputV1, ChatMessageV1, ChatRequestV1, ChatResponseV1, ChatStreamEventV1,
    ContentPart, FinishReasonV1, MessageContent, ToolCallV1, ToolChoiceMode, ToolChoiceV1,
    UsageCompleteness, UsageV2,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) struct TypedAnthropic {
    raw: Arc<dyn Provider>,
}

impl TypedAnthropic {
    pub(crate) fn new(raw: Arc<dyn Provider>) -> Self {
        Self { raw }
    }
}

#[async_trait]
impl ChatProvider for TypedAnthropic {
    fn slug(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_anthropic_request(&request)?;
        let response = self.raw.complete(provider_request(&request, body)).await?;
        let mut decoded = decode_anthropic_response(response.body, response.usage, &request.model)?;
        if !request.include_native_response {
            // G8: the native body is debug metadata, not contract. Decoded
            // extensions (e.g. "reasoning") always survive.
            decoded.extensions.remove("anthropic");
        }
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        Ok(decoded)
    }

    async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_anthropic_request(&request)?;
        let raw = self.raw.stream(provider_request(&request, body)).await?;
        Ok(decode_anthropic_stream(raw, request.model))
    }
}

pub fn encode_anthropic_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request.validate().map_err(ProviderError::InvalidRequest)?;
    let mut body = request
        .extensions
        .get("anthropic")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let native_system = body.get("system").and_then(Value::as_array).cloned();
    let native_tools = body.get("tools").and_then(Value::as_array).cloned();
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &request.messages {
        match message {
            ChatMessageV1::Developer { content, .. } | ChatMessageV1::System { content, .. } => {
                system.push(json!({"type":"text", "text":text_only(content)?}));
            }
            ChatMessageV1::User { content, .. } => {
                messages.push(json!({"role":"user", "content":anthropic_content(content)?}));
            }
            ChatMessageV1::Assistant {
                content,
                tool_calls,
                refusal,
                ..
            } => {
                let mut blocks = Vec::new();
                if let Some(content) = content {
                    blocks.extend(anthropic_content(content)?);
                }
                if let Some(refusal) = refusal {
                    blocks.push(json!({"type":"text", "text":refusal}));
                }
                for call in tool_calls {
                    let input: Value = serde_json::from_str(&call.arguments).map_err(|error| {
                        ProviderError::InvalidRequest(format!(
                            "tool call {} arguments are not valid JSON: {error}",
                            call.id
                        ))
                    })?;
                    blocks.push(json!({
                        "type":"tool_use", "id":call.id, "name":call.name, "input":input
                    }));
                }
                messages.push(json!({"role":"assistant", "content":blocks}));
            }
            ChatMessageV1::Tool {
                content,
                tool_call_id,
            } => messages.push(json!({
                "role":"user",
                "content":[{"type":"tool_result", "tool_use_id":tool_call_id,
                    "content":anthropic_content(content)?}]
            })),
            ChatMessageV1::Function { .. } => {
                return Err(ProviderError::InvalidRequest(
                    "Anthropic Messages does not support legacy function-role results; use tool"
                        .into(),
                ));
            }
        }
    }
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert("messages".into(), Value::Array(messages));
    if !system.is_empty() {
        if let Some(native_system) = native_system {
            for (index, block) in system.iter_mut().enumerate() {
                if let Some(cache_control) = native_system
                    .get(index)
                    .and_then(|native| native.get("cache_control"))
                {
                    block["cache_control"] = cache_control.clone();
                }
            }
        }
        body.insert("system".into(), Value::Array(system));
    }
    let max_tokens = request.max_output_tokens.ok_or_else(|| {
        ProviderError::InvalidRequest("Anthropic requires max_output_tokens".into())
    })?;
    body.insert("max_tokens".into(), max_tokens.into());
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(stop) = &request.stop {
        body.insert("stop_sequences".into(), json!(stop));
    }
    if !request.tools.is_empty() {
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name":tool.name,
                    "description":tool.description,
                    "input_schema":tool.parameters,
                })
            })
            .collect();
        if let Some(native_tools) = native_tools {
            for tool in &mut tools {
                let name = tool.get("name").and_then(Value::as_str);
                if let Some(cache_control) = native_tools.iter().find_map(|native| {
                    (native.get("name").and_then(Value::as_str) == name)
                        .then(|| native.get("cache_control"))
                        .flatten()
                }) {
                    tool["cache_control"] = cache_control.clone();
                }
            }
        }
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = &request.tool_choice {
        match choice {
            ToolChoiceV1::Mode(ToolChoiceMode::None) => {
                body.remove("tools");
            }
            ToolChoiceV1::Mode(ToolChoiceMode::Auto) => {
                body.insert("tool_choice".into(), json!({"type":"auto"}));
            }
            ToolChoiceV1::Mode(ToolChoiceMode::Required) => {
                body.insert("tool_choice".into(), json!({"type":"any"}));
            }
            ToolChoiceV1::Function { name } => {
                body.insert("tool_choice".into(), json!({"type":"tool", "name":name}));
            }
        }
    }
    // W3d/G7: Anthropic extended thinking is `{type: enabled, budget_tokens}`.
    // Typed field wins over an extensions-carried duplicate (inserted after
    // the extensions clone). `reasoning_effort` has no Anthropic analogue —
    // explicitly ignored (consumer-decision row).
    if let Some(thinking) = &request.thinking {
        if thinking.enabled {
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), Value::String("enabled".into()));
            if let Some(budget) = thinking.budget_tokens {
                obj.insert("budget_tokens".into(), json!(budget));
            }
            body.insert("thinking".into(), Value::Object(obj));
        } else {
            body.remove("thinking");
        }
    }
    Ok(Value::Object(body))
}

fn text_only(content: &MessageContent) -> Result<String, ProviderError> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => Ok(text.as_str()),
                _ => Err(ProviderError::InvalidRequest(
                    "Anthropic system/developer messages support text parts only".into(),
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| parts.join("\n")),
    }
}

fn anthropic_content(content: &MessageContent) -> Result<Vec<Value>, ProviderError> {
    let parts = match content {
        MessageContent::Text(text) => return Ok(vec![json!({"type":"text", "text":text})]),
        MessageContent::Parts(parts) => parts,
    };
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => Ok(json!({"type":"text", "text":text})),
            ContentPart::ImageUrl { image_url, .. } => {
                if let Some(data) = image_url.strip_prefix("data:") {
                    let (media_type, encoded) = data.split_once(";base64,").ok_or_else(|| {
                        ProviderError::InvalidRequest("invalid base64 image data URL".into())
                    })?;
                    Ok(json!({"type":"image", "source":{
                        "type":"base64", "media_type":media_type, "data":encoded
                    }}))
                } else {
                    Ok(json!({"type":"image", "source":{"type":"url", "url":image_url}}))
                }
            }
            ContentPart::InputAudio { .. } | ContentPart::File { .. } => {
                Err(ProviderError::InvalidRequest(
                    "Anthropic codec does not support audio/file content parts yet".into(),
                ))
            }
        })
        .collect()
}

pub fn decode_anthropic_response(
    body: Value,
    parsed_usage: ParsedUsage,
    requested_model: &str,
) -> Result<ChatResponseV1, ProviderError> {
    let blocks = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::Transport("Anthropic response has no content array".into())
        })?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("thinking") => {
                reasoning.push_str(block.get("thinking").and_then(Value::as_str).unwrap_or(""))
            }
            Some("tool_use") => tool_calls.push(ToolCallV1 {
                id: block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                arguments: serde_json::to_string(block.get("input").unwrap_or(&Value::Null))
                    .map_err(|error| ProviderError::Transport(error.to_string()))?,
                extensions: BTreeMap::new(),
            }),
            _ => {}
        }
    }
    let mut extensions = BTreeMap::from([("anthropic".into(), body.clone())]);
    if !reasoning.is_empty() {
        extensions.insert("reasoning".into(), Value::String(reasoning));
    }
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
            refusal: None,
        },
        finish_reason: body
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(decode_stop_reason),
        usage: UsageV2::from(parsed_usage),
        extensions,
    })
}

fn decode_stop_reason(reason: &str) -> FinishReasonV1 {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => FinishReasonV1::Stop,
        "max_tokens" | "model_context_window_exceeded" => FinishReasonV1::Length,
        "tool_use" => FinishReasonV1::ToolCalls,
        _ => FinishReasonV1::Unknown,
    }
}

fn decode_anthropic_stream(mut raw: ByteStream, requested_model: String) -> ChatEventStream {
    use futures_util::StreamExt;
    let stream = async_stream::try_stream! {
        let mut buffer = Vec::<u8>::new();
        let mut started = false;
        let mut open_tools = BTreeSet::<u32>::new();
        let mut emitted_usage = false;
        // The last running total published, so progress is emitted on change rather than per chunk.
        let mut last_running: Option<crate::ParsedUsage> = None;
        while let Some(chunk) = raw.next().await {
            let chunk = chunk?;
            let attempts = chunk.attempts;
            if !chunk.data.is_empty() {
                buffer.extend_from_slice(&chunk.data);
                while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let Some(value) = crate::sse_data_json(&line) else { continue; };
                    match value.get("type").and_then(Value::as_str) {
                        Some("message_start") if !started => {
                            let message = value.get("message").unwrap_or(&Value::Null);
                            yield ChatStreamEventV1::ResponseStart {
                                id: message.get("id").and_then(Value::as_str).map(str::to_owned),
                                model: message.get("model").and_then(Value::as_str)
                                    .unwrap_or(&requested_model).to_owned(),
                            };
                            started = true;
                        }
                        Some("content_block_start") => {
                            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                            let block = value.get("content_block").unwrap_or(&Value::Null);
                            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                                open_tools.insert(index);
                                yield ChatStreamEventV1::ToolCallStart {
                                    index,
                                    id: block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                                    name: block.get("name").and_then(Value::as_str).unwrap_or("").into(),
                                };
                            }
                        }
                        Some("content_block_delta") => {
                            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                            let delta = value.get("delta").unwrap_or(&Value::Null);
                            match delta.get("type").and_then(Value::as_str) {
                                Some("text_delta") => yield ChatStreamEventV1::TextDelta {
                                    delta: delta.get("text").and_then(Value::as_str).unwrap_or("").into()
                                },
                                Some("thinking_delta") => yield ChatStreamEventV1::ReasoningDelta {
                                    delta: delta.get("thinking").and_then(Value::as_str).unwrap_or("").into()
                                },
                                Some("input_json_delta") => yield ChatStreamEventV1::ToolCallArgumentsDelta {
                                    index,
                                    delta: delta.get("partial_json").and_then(Value::as_str).unwrap_or("").into()
                                },
                                _ => {}
                            }
                        }
                        Some("content_block_stop") => {
                            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                            if open_tools.remove(&index) {
                                yield ChatStreamEventV1::ToolCallEnd { index };
                            }
                        }
                        Some("message_delta") => {
                            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                                yield ChatStreamEventV1::Finish { reason: decode_stop_reason(reason) };
                            }
                        }
                        _ => {}
                    }
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
                // Anthropic is an `Incremental` family (TD-0013 D1): `message_start` carries input
                // and the full cache split before any content, and `message_delta` carries
                // cumulative output. Surfacing that as a non-final `Usage` is what lets an
                // interrupted stream settle the real numbers instead of a byte estimate — on a
                // cached prompt those categories *are* the bill. Emitted only when the totals
                // actually move, so a long stream does not carry one event per chunk.
                //
                // `Partial` is load-bearing here: the proxy treats a non-final `Usage` as
                // accounting-only, so it never supersedes the terminal frame and never reaches
                // the client (TD-0013 D7).
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
    #[test]
    fn native_body_gate_preserves_the_reasoning_extension() {
        // What complete() does under include_native_response=false: only the
        // family key goes; decoded-content extensions survive.
        let mut out = decode_anthropic_response(
            serde_json::json!({
                "id": "m1",
                "model": "claude-x",
                "content": [
                    {"type": "thinking", "thinking": "consider"},
                    {"type": "text", "text": "hi"}
                ],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
            ParsedUsage::default(),
            "claude-x",
        )
        .unwrap();
        assert!(out.extensions.contains_key("anthropic"));
        out.extensions.remove("anthropic");
        assert_eq!(out.extensions["reasoning"], "consider");
    }

    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;

    #[test]
    fn w3d_thinking_maps_to_native_and_effort_is_ignored() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model": "claude-test", "max_output_tokens": 128,
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
            "thinking": {"enabled": true, "budget_tokens": 4096}
        }))
        .unwrap();
        let body = encode_anthropic_request(&request).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
        // Anthropic has no effort concept — the typed field is dropped, not leaked.
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn request_codec_maps_system_tools_and_tool_results() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model":"claude-test", "max_output_tokens":128,
            "messages":[
                {"role":"developer","content":"policy"},
                {"role":"user","content":"use tool"},
                {"role":"assistant","tool_calls":[{"id":"c1","name":"lookup","arguments":"{\"q\":1}"}]},
                {"role":"tool","content":"done","tool_call_id":"c1"}
            ],
            "tools":[{"name":"lookup","parameters":{"type":"object"}}],
            "extensions":{"anthropic":{
                "system":[{"type":"text","text":"policy","cache_control":{"type":"ephemeral"}}],
                "tools":[{"name":"lookup","cache_control":{"type":"ephemeral"}}]
            }}
        })).unwrap();
        let body = encode_anthropic_request(&request).unwrap();
        assert_eq!(body["system"][0]["text"], "policy");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn response_codec_maps_text_thinking_tools_and_cache_usage() {
        let body = json!({
            "id":"m1", "model":"claude-test", "stop_reason":"tool_use",
            "content":[
                {"type":"thinking","thinking":"consider"},
                {"type":"text","text":"answer"},
                {"type":"tool_use","id":"c1","name":"lookup","input":{"q":1}}
            ]
        });
        let out = decode_anthropic_response(
            body,
            ParsedUsage {
                tokens_in: 2,
                tokens_out: 3,
                cache_creation_tokens: 4,
                cache_read_tokens: 5,
                reasoning_tokens: 0,
            },
            "fallback",
        )
        .unwrap();
        assert_eq!(
            out.output.content,
            Some(MessageContent::Text("answer".into()))
        );
        assert_eq!(out.output.tool_calls[0].arguments, "{\"q\":1}");
        assert_eq!(out.finish_reason, Some(FinishReasonV1::ToolCalls));
        assert_eq!(out.usage.cache_read_tokens, 5);
        assert_eq!(out.extensions["reasoning"], "consider");
    }

    #[tokio::test]
    async fn stream_codec_is_invariant_across_arbitrary_byte_boundaries() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-test\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"lookup\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ).as_bytes();
        for split in 0..=sse.len() {
            let raw: ByteStream = Box::pin(futures_util::stream::iter(vec![
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse[..split]),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse[split..]),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::new(),
                    usage: Some(ParsedUsage {
                        tokens_in: 2,
                        tokens_out: 3,
                        cache_creation_tokens: 4,
                        cache_read_tokens: 5,
                        reasoning_tokens: 0,
                    }),
                    usage_running: None,
                    attempts: 1,
                }),
            ]));
            let events = decode_anthropic_stream(raw, "fallback".into())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(
                matches!(
                    events.first(),
                    Some(ChatStreamEventV1::ResponseStart { .. })
                ),
                "split {split}"
            );
            assert!(events.iter().any(|event| matches!(event, ChatStreamEventV1::ToolCallStart { id, .. } if id == "c1")), "split {split}");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ChatStreamEventV1::ToolCallEnd { index: 0 })),
                "split {split}"
            );
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    ChatStreamEventV1::Finish {
                        reason: FinishReasonV1::ToolCalls
                    }
                )),
                "split {split}"
            );
            // Exactly one here because this raw stream is hand-built with `usage_running: None`
            // on its data chunks, i.e. it models a terminal-only family. For a real Anthropic
            // stream the count is higher — progress events precede the verdict — which
            // `streaming_usage_progress_tests` pins against the production primitive. What is
            // invariant across both is *one `Final`*, not one `Usage`.
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ChatStreamEventV1::Usage { .. }))
                    .count(),
                1,
                "split {split}"
            );
        }
    }
}

#[cfg(test)]
mod streaming_usage_progress_tests {
    //! TD-0013 — what a consumer of the *typed* stream now sees for an `Incremental` family.
    //!
    //! The chunk-boundary test above hand-builds its raw stream with `usage_running: None`, so it
    //! never exercises this path — and it asserts exactly one `Usage` event, which would otherwise
    //! read as a guarantee that no longer holds. This drives a real Anthropic fixture through the
    //! production `metered_passthrough` primitive instead, and pins the rule that actually applies.

    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;

    async fn decode_real_fixture() -> Vec<ChatStreamEventV1> {
        let body: &[u8] = include_bytes!("../tests/fixtures/anthropic/stream_cache_split.sse");
        // One SSE frame per chunk, as a paced upstream delivers them.
        let frames: Vec<Bytes> = String::from_utf8_lossy(body)
            .split_inclusive("\n\n")
            .map(|frame| Bytes::copy_from_slice(frame.as_bytes()))
            .collect();
        let upstream = futures_util::stream::iter(
            frames
                .into_iter()
                .map(Ok::<Bytes, reqwest::Error>)
                .collect::<Vec<_>>(),
        );
        let raw =
            crate::metered_passthrough(Box::pin(upstream), crate::anthropic::sniff_usage_line);
        let mut out = decode_anthropic_stream(raw, "claude-test".into());
        let mut events = Vec::new();
        while let Some(event) = out.next().await {
            events.push(event.unwrap());
        }
        events
    }

    /// **The contract rule, stated once.** Exactly one `Final` usage — the verdict — preceded by
    /// zero or more `Partial` ones carrying progress. A consumer must treat a non-final `Usage` as
    /// an update, never as the end of the call; the proxy does exactly that (TD-0013 D7).
    #[tokio::test]
    async fn an_incremental_family_emits_progress_then_exactly_one_verdict() {
        let events = decode_real_fixture().await;
        let usages: Vec<&UsageV2> = events
            .iter()
            .filter_map(|event| match event {
                ChatStreamEventV1::Usage { usage } => Some(usage),
                _ => None,
            })
            .collect();

        let finals = usages
            .iter()
            .filter(|usage| usage.completeness == UsageCompleteness::Final)
            .count();
        assert_eq!(
            finals, 1,
            "there must be exactly one authoritative usage per logical call"
        );
        assert_eq!(
            usages.last().map(|usage| usage.completeness),
            Some(UsageCompleteness::Final),
            "the verdict must arrive last, so a consumer taking the latest value is correct"
        );

        let partials: Vec<&&UsageV2> = usages
            .iter()
            .filter(|usage| usage.completeness == UsageCompleteness::Partial)
            .collect();
        assert!(
            !partials.is_empty(),
            "Anthropic reports input and the cache split on message_start; not surfacing it is \
             the defect TD-0013 removed"
        );

        // The first progress event already carries the whole prompt cost — this is what makes an
        // interrupted stream settleable.
        let first = partials[0];
        assert_eq!(first.tokens_in, 1024);
        assert_eq!(first.cache_creation_tokens, 2048);
        assert_eq!(first.cache_read_tokens, 4096);
    }

    /// Progress must not repeat unchanged: a long stream must not carry one usage event per chunk.
    #[tokio::test]
    async fn progress_is_emitted_on_change_not_per_chunk() {
        let events = decode_real_fixture().await;
        let usage_events = events
            .iter()
            .filter(|event| matches!(event, ChatStreamEventV1::Usage { .. }))
            .count();
        let data_events = events.len() - usage_events;
        assert!(
            usage_events < data_events,
            "usage events ({usage_events}) must not dominate the stream ({data_events} others)"
        );
    }
}
