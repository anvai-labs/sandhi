//! Bounded SSE attachments for native typed streams. All admitted sources use
//! complete-event validation. Native delivery assembles complete events,
//! including multiline data and opaque/unknown fields, with normalized LF framing.

#[cfg(test)]
mod fidelity_tests {
    use futures_util::StreamExt;
    use sandhi_core::{
        ChatStreamEventV1 as Event, EndpointFamilyV1 as Family, UsageCompleteness as Complete,
    };
    use serde_json::{json, Value};

    fn frames(family: Family) -> Vec<Value> {
        match family {
            Family::OpenaiChatCompletions => vec![
                json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"ok","vendor_state":"opaque"},"finish_reason":null}]}),
                json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                json!({"object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,
                    "prompt_tokens_details":{"cached_tokens":60,"cache_write_tokens":30}}}),
            ],
            Family::OpenaiResponses => vec![
                json!({"type":"response.output_text.delta","delta":"ok"}),
                json!({"type":"response.completed","response":{"id":"resp_fixture","status":"completed","model":"fixture",
                    "output":[{"type":"reasoning","encrypted_content":"opaque"},{"type":"message","content":[{"type":"output_text","text":"ok"}]}],
                    "usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":60,"cache_write_tokens":30}}}}),
            ],
            Family::AnthropicMessages => vec![
                json!({"type":"message_start","message":{"type":"message","role":"assistant","id":"msg_fixture","model":"fixture",
                    "usage":{"input_tokens":10,"cache_read_input_tokens":60,"cache_creation_input_tokens":30}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}),
                json!({"type":"message_stop"}),
            ],
            Family::GeminiGenerateContent => vec![
                json!({"candidates":[{"content":{"parts":[{"text":"ok","thoughtSignature":"opaque"}]},"finishReason":"STOP"}],
                    "usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":60,"candidatesTokenCount":20,"totalTokenCount":120}}),
            ],
            _ => unreachable!(),
        }
    }

    fn wire(family: Family, multiline: bool, newline: &str) -> String {
        let mut wire = String::new();
        for value in frames(family) {
            wire.push_str("id: frame-fixture\n");
            if let Some(kind) = value.get("type").and_then(Value::as_str) {
                wire.push_str(&format!("event: {kind}\n"));
            }
            let json = if multiline {
                serde_json::to_string_pretty(&value).unwrap()
            } else {
                value.to_string()
            };
            for line in json.lines() {
                wire.push_str(&format!("data: {line}\n"));
            }
            wire.push('\n');
        }
        if family == Family::OpenaiChatCompletions {
            wire.push_str("data: [DONE]\n\n");
        }
        wire.replace('\n', newline)
    }

    fn decode(family: Family, raw: crate::ByteStream) -> crate::ChatEventStream {
        crate::qualified_stream::fixture(raw, "fixture".into(), family, true)
    }

    #[tokio::test]
    async fn native_state_and_usage_require_clean_http_eof_at_every_split() {
        for family in [
            Family::OpenaiChatCompletions,
            Family::OpenaiResponses,
            Family::AnthropicMessages,
            Family::GeminiGenerateContent,
        ] {
            for multiline in [false, true] {
                for newline in ["\n", "\r\n", "\r"] {
                    let wire = wire(family, multiline, newline);
                    for split in 0..=wire.len() {
                        let incoming = futures_util::stream::iter(vec![
                            Ok::<_, reqwest::Error>(bytes::Bytes::copy_from_slice(
                                &wire.as_bytes()[..split],
                            )),
                            Ok(bytes::Bytes::copy_from_slice(&wire.as_bytes()[split..])),
                        ]);
                        let raw = match family {
                            Family::OpenaiChatCompletions => crate::metered_openai_passthrough(
                                incoming,
                                crate::openai::sniff_usage_line,
                            ),
                            Family::OpenaiResponses => crate::metered_passthrough(
                                incoming,
                                crate::openai_responses::sniff_responses_usage_line,
                            ),
                            Family::AnthropicMessages => crate::metered_passthrough(
                                incoming,
                                crate::anthropic::sniff_usage_line,
                            ),
                            Family::GeminiGenerateContent => crate::metered_passthrough(
                                incoming,
                                crate::gemini::sniff_usage_line,
                            ),
                            _ => unreachable!(),
                        };
                        let events: Vec<_> = tokio::time::timeout(std::time::Duration::from_secs(1),
                            decode(family,raw).collect()).await
                            .unwrap_or_else(|_| panic!("completed source did not finish: {family:?}/{multiline}/{newline:?}/{split}"));
                        let events: Vec<_> = events
                            .into_iter()
                            .map(|event| {
                                event.unwrap_or_else(|error| panic!("{family:?}: {error:?}"))
                            })
                            .collect();
                        assert!(events.iter().all(|event| !matches!(event,Event::Usage {usage}
                            if usage.cache_read_observation.is_some_and(|observation| observation.status==sandhi_core::CacheReadStatus::Malformed))),
                            "valid multiline cache metadata became malformed: {family:?}/{multiline}/{newline:?}/{split}");
                        assert!(
                            matches!(events.last(), Some(Event::Finish { .. })),
                            "{family:?}/{split}"
                        );
                        let finals: Vec<_> = events
                            .iter()
                            .filter_map(|event| match event {
                                Event::Usage { usage } if usage.completeness == Complete::Final => {
                                    Some(usage)
                                }
                                _ => None,
                            })
                            .collect();
                        assert_eq!(
                            finals.len(),
                            1,
                            "{family:?}/{multiline}/{newline:?}/{split}"
                        );
                        let usage = finals[0];
                        assert_eq!(
                            usage.tokens_in,
                            if family == Family::GeminiGenerateContent {
                                40
                            } else {
                                10
                            }
                        );
                        assert_eq!(usage.cache_read_tokens, 60);
                        assert_eq!(
                            usage.cache_creation_tokens,
                            if family == Family::GeminiGenerateContent {
                                0
                            } else {
                                30
                            }
                        );
                        assert_eq!(usage.tokens_out, 20);
                        assert_eq!(
                            usage.cache_read_observation.unwrap().status,
                            sandhi_core::CacheReadStatus::Reported
                        );
                        let native: Vec<_> = events
                            .iter()
                            .filter_map(|event| match event {
                                Event::NativeFrame { frame } => Some(frame),
                                _ => None,
                            })
                            .collect();
                        assert!(native.iter().any(|frame| frame.sse.contains("opaque")));
                        assert!(native.iter().all(|frame| frame.endpoint_family == family));
                        assert!(native[0].sse.contains("id: frame-fixture"));
                        assert_eq!(
                            native.len(),
                            frames(family).len()
                                + usize::from(family == Family::OpenaiChatCompletions)
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn native_transport_error_preserves_partial_usage_without_success() {
        for family in [Family::AnthropicMessages, Family::GeminiGenerateContent] {
            let value = if family == Family::AnthropicMessages {
                frames(family)[0].clone()
            } else {
                json!({"candidates":[{"content":{"parts":[{"text":"ok","thoughtSignature":"opaque"}]}}],
                    "usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":60,"candidatesTokenCount":2}})
            };
            let raw = Box::pin(futures_util::stream::iter(vec![
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::from(format!("data: {value}\n\n")),
                    attempts: 1,
                    ..Default::default()
                }),
                Err(crate::ProviderError::Transport("fixture abort".into())),
            ]));
            let events: Vec<_> = decode(family, raw).collect().await;
            assert!(events.last().unwrap().is_err());
            assert!(!events
                .iter()
                .any(|event| matches!(event, Ok(Event::Finish { .. }))));
            assert!(!events.iter().any(|event|matches!(event,Ok(Event::Usage {usage}) if usage.completeness==Complete::Final)));
            let native_index = events
                .iter()
                .position(|event| matches!(event, Ok(Event::NativeFrame { .. })))
                .unwrap();
            assert!(events[..native_index].iter().any(
                |event| matches!(event,Ok(Event::Usage {usage}) if usage.cache_read_tokens==60)
            ));
        }
    }

    #[tokio::test]
    async fn gemini_native_eof_without_counts_never_invents_measured_success() {
        for value in [
            json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"ok"}]}}]}),
            json!({"promptFeedback":{"blockReason":"SAFETY"}}),
        ] {
            let raw = Box::pin(futures_util::stream::iter(vec![Ok(crate::StreamChunk {
                data: bytes::Bytes::from(format!("data: {value}\n\n")),
                ..Default::default()
            })]));
            let events = decode(Family::GeminiGenerateContent, raw)
                .collect::<Vec<_>>()
                .await;
            assert!(events.last().is_some_and(Result::is_err));
            assert!(!events
                .iter()
                .any(|event| matches!(event, Ok(Event::Finish { .. }))));
            assert!(!events.iter().any(|event|matches!(event,Ok(Event::Usage{usage}) if usage.completeness==Complete::Final)));
        }
    }

    #[tokio::test]
    async fn native_truncated_eof_never_manufactures_protocol_completion() {
        for family in [
            Family::OpenaiChatCompletions,
            Family::OpenaiResponses,
            Family::AnthropicMessages,
            Family::GeminiGenerateContent,
        ] {
            let mut value = frames(family)[0].clone();
            if family == Family::GeminiGenerateContent {
                value["candidates"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("finishReason");
            }
            let raw = Box::pin(futures_util::stream::iter(vec![Ok(crate::StreamChunk {
                data: bytes::Bytes::from(format!("data: {value}\n\n")),
                attempts: 2,
                ..Default::default()
            })]));
            let events: Vec<_> = decode(family, raw).collect().await;
            assert!(
                events.last().is_some_and(Result::is_err),
                "{family:?}: {events:?}"
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(Event::Finish { .. }))),
                "{family:?}"
            );
            assert!(!events.iter().any(|event|matches!(event,Ok(Event::Usage {usage}) if usage.completeness==Complete::Final)),"{family:?}");
        }
    }

    #[tokio::test]
    async fn native_protocol_error_is_preserved_without_success_or_eof_wait() {
        for family in [
            Family::OpenaiChatCompletions,
            Family::OpenaiResponses,
            Family::AnthropicMessages,
            Family::GeminiGenerateContent,
        ] {
            let value =
                json!({"type":"error","error":{"message":"fixture overload","opaque":"preserved"}});
            let raw = Box::pin(
                futures_util::stream::iter(vec![Ok(crate::StreamChunk {
                    data: bytes::Bytes::from(format!("data: {value}\n\n")),
                    attempts: 2,
                    ..Default::default()
                })])
                .chain(futures_util::stream::pending()),
            );
            let events: Vec<_> = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                decode(family, raw).collect(),
            )
            .await
            .unwrap();
            assert!(events
                .last()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("fixture overload"));
            assert!(!events
                .iter()
                .any(|event| matches!(event, Ok(Event::Finish { .. }))));
            let native: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Ok(Event::NativeFrame { frame }) => Some(frame),
                    _ => None,
                })
                .collect();
            assert_eq!(native.len(), 1);
            assert!(native[0].sse.contains("preserved"));
        }
    }
}

use crate::{linesplit::LineSplitter, ProviderError, MAX_STREAM_LINE_BYTES};
use sandhi_core::{EndpointFamilyV1, NativeStreamFrameV1};

pub(crate) fn upstream_error(value: &serde_json::Value) -> Option<ProviderError> {
    let error = value.get("error").filter(|error| !error.is_null())?;
    let message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("upstream stream error");
    Some(ProviderError::Transport(message.to_owned()))
}

pub(crate) struct SseDecoder {
    lines: LineSplitter,
    family: Option<EndpointFamilyV1>,
    wire: Vec<u8>,
    data: Vec<u8>,
    has_data: bool,
    first_line: bool,
}
type DecodedSseLine = (Vec<u8>, Option<NativeStreamFrameV1>);
impl SseDecoder {
    pub(crate) fn new(family: Option<EndpointFamilyV1>) -> Self {
        Self {
            lines: if family.is_some() {
                LineSplitter::new_sse(MAX_STREAM_LINE_BYTES)
            } else {
                LineSplitter::new(MAX_STREAM_LINE_BYTES)
            },
            family,
            wire: Vec::new(),
            data: Vec::new(),
            has_data: false,
            first_line: true,
        }
    }
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.lines.push(bytes);
    }
    pub(crate) fn over_budget(&self) -> bool {
        self.lines.over_budget() || self.wire.len() > MAX_STREAM_LINE_BYTES
    }
    pub(crate) fn reset(&mut self) {
        self.lines.reset();
        self.wire.clear();
        self.data.clear();
        self.has_data = false;
    }
    pub(crate) fn flush_newline(&mut self) -> bool {
        if self.family.is_none() {
            return self.lines.flush_newline();
        }
        let pending = self.lines.flush_newline() || !self.wire.is_empty();
        if pending {
            self.lines.push(b"\n\n");
        }
        pending
    }
    pub(crate) fn next_line(&mut self) -> Result<Option<DecodedSseLine>, ProviderError> {
        while let Some(line) = self.lines.next_line() {
            let Some(family) = self.family else {
                return Ok(Some((line, None)));
            };
            let line = line.strip_suffix(b"\n").unwrap_or(&line);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let line = if std::mem::replace(&mut self.first_line, false) {
                line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line)
            } else {
                line
            };
            if self.wire.len().saturating_add(line.len()).saturating_add(1) > MAX_STREAM_LINE_BYTES
            {
                return Err(ProviderError::Transport(
                    "native SSE event exceeds the stream frame limit".into(),
                ));
            }
            if line.is_empty() {
                if self.wire.is_empty() {
                    continue;
                }
                self.wire.push(b'\n');
                let wire = String::from_utf8(std::mem::take(&mut self.wire)).map_err(|_| {
                    ProviderError::Transport("native SSE event is not UTF-8".into())
                })?;
                let mut projected = b"data: ".to_vec();
                if self.has_data {
                    projected.extend_from_slice(&self.data);
                } else {
                    projected.clear();
                }
                self.data.clear();
                self.has_data = false;
                return Ok(Some((
                    projected,
                    Some(NativeStreamFrameV1 {
                        endpoint_family: family,
                        sse: wire,
                    }),
                )));
            }
            self.wire.extend_from_slice(line);
            self.wire.push(b'\n');
            if let Some(data) = line.strip_prefix(b"data:") {
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.data
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
                self.has_data = true;
            } else if line == b"data" {
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.has_data = true;
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
pub(crate) fn json(frame: &NativeStreamFrameV1) -> Option<serde_json::Value> {
    frame.json_data()
}

// Framing invariants are independent of source counters and settlement.
// The production source observer is exercised by the fidelity tests above.

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_multiline_events_are_invariant_at_every_byte_boundary() {
        for newline in ["\n", "\r\n", "\r"] {
            let wire = "event: future\nid: fixture\ndata: {\"type\":\"future\",\ndata: \"opaque\":\"signed\"}\n\n".replace('\n',newline);
            for split in 0..=wire.len() {
                let mut decoder = SseDecoder::new(Some(EndpointFamilyV1::AnthropicMessages));
                let mut events = Vec::new();
                for bytes in [&wire.as_bytes()[..split], &wire.as_bytes()[split..]] {
                    decoder.push(bytes);
                    while let Some((_, native)) = decoder.next_line().unwrap() {
                        events.push(native.unwrap());
                    }
                }
                assert_eq!(events.len(), 1, "{newline:?}/{split}");
                assert_eq!(json(&events[0]).unwrap()["opaque"], "signed");
                assert!(events[0].sse.contains("id: fixture\n"));
            }
        }
    }
    #[test]
    fn native_event_limit_also_bounds_many_short_data_lines() {
        let mut decoder = SseDecoder::new(Some(EndpointFamilyV1::OpenaiResponses));
        let line = format!("data: {}\n", "x".repeat(1024));
        let mut rejected = false;
        for _ in 0..(MAX_STREAM_LINE_BYTES / 1024 + 2) {
            decoder.push(line.as_bytes());
            if decoder.next_line().is_err() {
                rejected = true;
                break;
            }
        }
        assert!(rejected);
        assert!(decoder.wire.len() <= MAX_STREAM_LINE_BYTES);
    }
}
