//! Typed projection of strict source evidence. No transport, retry or settlement.
use crate::{raw::QualifiedStreamObservation, AttemptOutcome, ChatEventStream, ProviderError};
use futures_util::StreamExt;
use sandhi_core::{ChatStreamEventV1, UsageCompleteness};

pub(crate) fn decode(
    inner: ChatEventStream,
    observation: QualifiedStreamObservation,
) -> ChatEventStream {
    Box::pin(async_stream::try_stream! {
        let mut inner=inner;
        let mut finish=None;
        let mut last_progress=None;
        while let Some(event)=inner.next().await {
            let mut event=Some(event.map_err(|error|crate::peer::stream_error(error,&observation)));
            if matches!(event.as_ref(),Some(Ok(ChatStreamEventV1::ResponseStart{..}))) {
                yield event.take().expect("response start")?;
            }
            // Publish actual response-scoped progress for embedded callers as
            // well as gateway owners. Final counters remain fenced by EOF.
            if let Some(usage)=observation.evidence().usage.filter(|usage|
                usage.completeness==UsageCompleteness::Partial)
            {
                if last_progress.as_ref()!=Some(&usage) {
                    last_progress=Some(usage.clone());
                    yield ChatStreamEventV1::Usage{usage};
                }
            }
            let Some(event)=event else {continue;};
            match event? {
                ChatStreamEventV1::Usage{usage} if usage.completeness==UsageCompleteness::Final => {
                    // A decoder's final marker is not source authority. Publish
                    // one actual final measurement only after EOF qualification.
                }
                ChatStreamEventV1::Finish{reason}=>finish=Some(reason),
                event=>yield event,
            }
        }
        drop(inner);
        let evidence=observation.evidence();
        if let Some(failure)=evidence.gateway_failure {Err(failure.into_error())?;}
        if evidence.snapshot.qualification_error.is_some()
            || !evidence.snapshot.protocol_done
            || evidence.snapshot.outcome!=Some(AttemptOutcome::Success)
            || !evidence.usage.as_ref().is_some_and(|usage| usage.completeness==UsageCompleteness::Final)
        {
            Err(ProviderError::OutputProjection(format!(
                "upstream stream evidence is unqualified (protocol_done={}, outcome={:?}, qualification_error={:?})",
                evidence.snapshot.protocol_done,evidence.snapshot.outcome,evidence.snapshot.qualification_error,
            )))?;
        }
        yield ChatStreamEventV1::Usage{usage:evidence.usage.expect("qualified final source usage")};
        if let Some(reason)=finish { yield ChatStreamEventV1::Finish{reason}; }
    })
}

/// Unit fixtures pass through the production source observer and projectors.
/// Their synthetic StreamChunk usage fields are deliberately not authority.
#[cfg(test)]
pub(crate) fn fixture(
    raw: crate::ByteStream,
    model: String,
    family: sandhi_core::EndpointFamilyV1,
    native: bool,
) -> ChatEventStream {
    Box::pin(async_stream::try_stream! {
        let source = crate::attempt::observe_qualified_stream(None, "fixture", Some(&model), family,
            Default::default(), Default::default(), |_| async move {
                let raw: crate::raw::RawChunkStream = Box::pin(raw.map(|chunk| chunk.map(|chunk| chunk.data)));
                Ok((raw, None))
            }).await?;
        let projected = match family {
            sandhi_core::EndpointFamilyV1::AnthropicMessages => crate::anthropic_typed::qualified::decode(source.stream, model, native),
            sandhi_core::EndpointFamilyV1::GeminiGenerateContent => crate::gemini_typed::qualified::decode(source.stream, model, native),
            sandhi_core::EndpointFamilyV1::OpenaiChatCompletions => crate::typed::decode_openai_stream_delivery(source.stream, model, native, true),
            sandhi_core::EndpointFamilyV1::OpenaiResponses => crate::openai_responses_typed::decode_responses_stream_delivery(source.stream, model, native, true),
            _ => unreachable!(),
        };
        let mut stream = decode(projected, source.observation);
        while let Some(event) = stream.next().await {yield event?;}
    })
}

#[cfg(test)]
mod drain_errors {
    use super::*;
    use sandhi_core::EndpointFamilyV1 as Family;
    use serde_json::json;
    #[tokio::test]
    async fn known_projection_refusal_remains_terminal_when_drain_times_out() {
        let mut failures = Vec::new();
        for family in [
            Family::OpenaiChatCompletions,
            Family::OpenaiResponses,
            Family::GeminiGenerateContent,
            Family::AnthropicMessages,
        ] {
            let value = match family {
                Family::OpenaiChatCompletions => {
                    json!({"object":"chat.completion.chunk","id":"r","model":"fixture","choices":[{"index":0,"delta":{"content":"reply","vendor_state":"opaque"},"finish_reason":null}]})
                }
                Family::OpenaiResponses => {
                    json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs","summary":[],"encrypted_content":"opaque"}})
                }
                Family::GeminiGenerateContent => {
                    json!({"candidates":[{"content":{"parts":[{"text":"reply","thoughtSignature":"opaque"}]}}]})
                }
                Family::AnthropicMessages => {
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"reply","signature":"opaque"}})
                }
                _ => unreachable!(),
            };
            let prefix = if family == Family::AnthropicMessages {
                format!(
                    "data: {}\n\n",
                    json!({"type":"message_start","message":{"type":"message","id":"msg","role":"assistant","content":[],"usage":{"input_tokens":2,"output_tokens":0,"cache_read_input_tokens":6,"cache_creation_input_tokens":2}}})
                )
            } else {
                String::new()
            };
            let wire = bytes::Bytes::from(format!("{prefix}data: {value}\n\n"));
            let source = crate::attempt::observe_qualified_stream(
                None,
                "fixture",
                Some("fixture"),
                family,
                Default::default(),
                Default::default(),
                |_| async move {
                    let raw: crate::raw::RawChunkStream =
                        Box::pin(futures_util::stream::iter(vec![
                            Ok(wire),
                            Err(ProviderError::Timeout(std::time::Duration::from_millis(10))),
                        ]));
                    Ok((raw, None))
                },
            )
            .await
            .unwrap();
            let evidence = source.observation.clone();
            let projected = match family {
                Family::OpenaiChatCompletions => {
                    crate::chat_projection::decode(source.stream, "fixture".into())
                }
                Family::OpenaiResponses => {
                    crate::responses_projection::decode(source.stream, "fixture".into(), false)
                }
                Family::GeminiGenerateContent => {
                    crate::gemini_typed::qualified::decode(source.stream, "fixture".into(), false)
                }
                Family::AnthropicMessages => crate::anthropic_typed::qualified::decode(
                    source.stream,
                    "fixture".into(),
                    false,
                ),
                _ => unreachable!(),
            };
            let actual = decode(projected, source.observation)
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                evidence.evidence().snapshot.outcome,
                Some(AttemptOutcome::Timeout)
            );
            assert!(!actual
                .iter()
                .any(|event| matches!(event, Ok(ChatStreamEventV1::Finish { .. }))));
            for error in actual.iter().filter_map(|event| event.as_ref().err()) {
                assert!(!error.as_typed(Some("fixture")).retryable);
            }
            if !actual
                .iter()
                .any(|e| matches!(e, Err(ProviderError::OutputProjection(_))))
            {
                failures.push(family);
            }
        }
        assert!(
            failures.is_empty(),
            "known refusals replaced with retryable drain failure: {failures:?}"
        );
    }
}
