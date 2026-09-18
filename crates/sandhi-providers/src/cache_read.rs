//! Carry field availability through canonical decoding without inventing measured counts.
//!
//! ResponseStart keeps its established position as the first canonical event. A consumer
//! dropping immediately after that event, before polling an observation or content, has not
//! received availability evidence yet; that narrow delivery boundary remains unknown.
//! Unavailable Usage events carry metadata-only corrections, not numeric measurements.
use crate::{ByteStream, ChatEventStream};
use futures_util::StreamExt;
use sandhi_core::{merge_cache_read_observation, CacheReadObservation, ChatStreamEventV1, UsageV2};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Observation {
    cache: Option<CacheReadObservation>,
    attempts: u32,
}

pub(crate) fn decode_with_observation(
    raw: ByteStream,
    model: String,
    decode: fn(ByteStream, String) -> ChatEventStream,
) -> ChatEventStream {
    let state = Arc::new(Mutex::new(Observation::default()));
    let incoming = Arc::clone(&state);
    let raw = raw.map(move |item| {
        if let Ok(chunk) = &item {
            let mut state = incoming.lock().expect("cache observation lock");
            state.attempts = chunk.attempts;
            merge_cache_read_observation(
                &mut state.cache,
                chunk.usage_running.and_then(|u| u.cache_read_observation),
            );
            merge_cache_read_observation(
                &mut state.cache,
                chunk.usage.and_then(|u| u.cache_read_observation),
            );
            merge_cache_read_observation(&mut state.cache, chunk.cache_read_observation);
        }
        item
    });
    let mut decoded = decode(Box::pin(raw), model);
    Box::pin(async_stream::stream! {
        let mut last: Option<UsageV2> = None;
        let mut published_observation = None;
        loop {
            let next = decoded.next().await;
            let (observation, attempts) = {
                let state = state.lock().expect("cache observation lock");
                (state.cache, state.attempts)
            };
            match next {
                Some(Ok(ChatStreamEventV1::Usage { mut usage })) => {
                    if observation.is_some() {
                        usage.cache_read_observation = observation;
                    }
                    last = Some(usage.clone());
                    published_observation = usage.cache_read_observation;
                    yield Ok(ChatStreamEventV1::Usage { usage });
                }
                other => {
                    // Metadata-only corrections precede content/finish/error, and keep every prior
                    // numeric value. Unavailable marks an accounting-only update, never a
                    // second final numeric verdict (or a translated provider usage frame).
                    // Publish before delivered content so cancellation immediately after that
                    // content cannot lose evidence already inspected in its upstream chunk.
                    let boundary = !matches!(&other, Some(Ok(ChatStreamEventV1::ResponseStart { .. })));
                    if boundary && observation.is_some() && published_observation != observation {
                        let mut usage = last.clone().unwrap_or_default();
                        usage.cache_read_observation = observation;
                        usage.attempts = attempts;
                        usage.completeness = sandhi_core::UsageCompleteness::Unavailable;
                        published_observation = observation;
                        yield Ok(ChatStreamEventV1::Usage { usage });
                    }
                    match other {
                        Some(event) => yield event,
                        None => break,
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParsedUsage, StreamChunk};
    use bytes::Bytes;
    use sandhi_core::{CacheReadFamily, CacheReadStatus, UsageCompleteness};
    use serde_json::json;

    fn status(chunk: &StreamChunk) -> Option<CacheReadStatus> {
        chunk.cache_read_observation.map(|o| o.status)
    }

    async fn openai_frames(frames: &[serde_json::Value]) -> Vec<StreamChunk> {
        let bytes: Vec<_> = frames
            .iter()
            .map(|frame| Ok::<_, reqwest::Error>(Bytes::from(format!("data: {frame}\n\n"))))
            .collect();
        crate::metered_passthrough(
            futures_util::stream::iter(bytes),
            crate::openai::sniff_usage_line,
        )
        .map(Result::unwrap)
        .collect()
        .await
    }

    #[test]
    fn buffered_absent_and_malformed_do_not_invent_numeric_observations() {
        for (body, expected) in [
            (json!({}), CacheReadStatus::Absent),
            (json!({"usage":null}), CacheReadStatus::Malformed),
            (
                json!({"usage":{"prompt_tokens":9,"prompt_tokens_details":{"cached_tokens":0}}}),
                CacheReadStatus::Reported,
            ),
        ] {
            let numeric_before = crate::parse_openai_usage(&body);
            let (response, numeric_after) = crate::buffered_usage(CacheReadFamily::OpenAi, &body);
            assert_eq!(numeric_before, numeric_after);
            assert_eq!(response.cache_read_observation.unwrap().status, expected);
        }
    }

    #[tokio::test]
    async fn null_placeholder_and_absent_stream_keep_numeric_none_and_bytes_exact() {
        let frame = json!({"choices":[{"delta":{"content":"hello"}}],"usage":null});
        let expected = format!("data: {frame}\n\n");
        let chunks = openai_frames(&[frame]).await;
        assert_eq!(chunks[0].data, expected);
        assert_eq!(status(&chunks[0]), Some(CacheReadStatus::Absent));
        assert_eq!(
            status(chunks.last().unwrap()),
            Some(CacheReadStatus::Absent)
        );
        assert!(chunks
            .iter()
            .all(|c| c.usage.is_none() && c.usage_running.is_none()));
    }

    #[tokio::test]
    async fn late_zero_then_partial_omission_then_terminal_null_preserves_numeric_semantics() {
        let frames = [
            json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":0}}}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}),
            json!({"choices":[],"usage":null}),
        ];
        let chunks = openai_frames(&frames).await;
        assert_eq!(status(&chunks[1]), Some(CacheReadStatus::Reported));
        assert_eq!(status(&chunks[2]), Some(CacheReadStatus::Reported));
        assert_eq!(status(&chunks[3]), Some(CacheReadStatus::Malformed));
        let final_usage = chunks.last().unwrap().usage.unwrap();
        assert_eq!(
            (
                final_usage.tokens_in,
                final_usage.tokens_out,
                final_usage.cache_read_tokens
            ),
            (10, 3, 0)
        );
        assert_eq!(
            final_usage.cache_read_observation.unwrap().status,
            CacheReadStatus::Malformed
        );
        let wire: Vec<_> = chunks.iter().flat_map(|c| c.data.iter().copied()).collect();
        let expected: String = frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect();
        assert_eq!(wire, expected.as_bytes());
    }

    #[tokio::test]
    async fn incremental_observation_survives_consumer_abort() {
        let frame = Bytes::from_static(b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9,\"cache_read_input_tokens\":0}}}\n\n");
        let upstream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(frame.clone())])
            .chain(futures_util::stream::pending());
        let mut raw =
            crate::metered_passthrough(Box::pin(upstream), crate::anthropic::sniff_usage_line);
        let first = raw.next().await.unwrap().unwrap();
        assert_eq!(first.data, frame);
        assert_eq!(status(&first), Some(CacheReadStatus::Reported));
        assert_eq!(first.usage_running.unwrap().tokens_in, 9);
        assert!(first.usage.is_none());
        drop(raw);
    }

    fn decode_usage(mut raw: ByteStream, _: String) -> ChatEventStream {
        Box::pin(async_stream::try_stream! {
            while let Some(chunk) = raw.next().await {
                if let Some(usage) = chunk?.usage {
                    yield ChatStreamEventV1::Usage { usage: usage.into() };
                }
            }
        })
    }

    #[tokio::test]
    async fn typed_metadata_without_numeric_usage_stays_unavailable() {
        let chunks = openai_frames(&[json!({"choices":[],"usage":null})]).await;
        let raw = Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok)));
        let events: Vec<_> = decode_with_observation(raw, "m".into(), decode_usage)
            .collect()
            .await;
        let ChatStreamEventV1::Usage { usage } = events[0].as_ref().unwrap() else {
            panic!("usage")
        };
        assert_eq!(usage.completeness, UsageCompleteness::Unavailable);
        assert_eq!(
            usage.cache_read_observation.unwrap().status,
            CacheReadStatus::Malformed
        );
        assert_eq!(usage.tokens_in, 0);
    }

    #[tokio::test]
    async fn typed_transport_error_retains_observed_cache_without_finalizing_counts() {
        let observation = Some(CacheReadObservation::origin(CacheReadStatus::Reported));
        let raw = Box::pin(futures_util::stream::iter(vec![
            Ok(StreamChunk {
                cache_read_observation: observation,
                usage_running: Some(ParsedUsage {
                    cache_read_observation: observation,
                    cache_read_tokens: 4,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            Err(crate::ProviderError::Transport("abort".into())),
        ]));
        let events: Vec<_> = decode_with_observation(raw, "m".into(), decode_usage)
            .collect()
            .await;
        let ChatStreamEventV1::Usage { usage } = events[0].as_ref().unwrap() else {
            panic!("usage")
        };
        assert_eq!(usage.cache_read_observation, observation);
        assert_eq!(usage.completeness, UsageCompleteness::Unavailable);
        // This deliberately terminal-only decoder never published its running numbers.
        // Availability must not fabricate a measurement that the numeric path did not emit.
        assert_eq!(usage.cache_read_tokens, 0);
        assert!(events[1].is_err());
    }
}
