//! Buffered typed delivery over one qualified SSE transport response.
use super::{
    aggregate_stream_for_delivery, decode_responses_stream_delivery, project_buffered_aggregate,
    provider_request_observed, TypedOpenAiResponses,
};
use crate::{AttemptContext, ProviderError, QualifiedCompletion, QualifiedStreamDelivery};
use sandhi_core::{ChatRequestV1, ChatResponseV1, UsageCompleteness};

pub(super) async fn complete(
    provider: &TypedOpenAiResponses,
    request: ChatRequestV1,
    headers: http::HeaderMap,
    context: Option<AttemptContext>,
) -> Result<QualifiedCompletion<ChatResponseV1>, ProviderError> {
    let body = provider.qualified_request(&request)?;
    let requested_model = request.model.clone();
    let transport = provider
        .raw
        .stream_qualified(
            provider_request_observed(&request, body, headers, context),
            QualifiedStreamDelivery::Buffered,
        )
        .await?;
    let decoded = if request.include_native_response {
        decode_responses_stream_delivery(transport.stream, request.model, true, true)
    } else {
        crate::responses_projection::decode(transport.stream, request.model, true)
    };
    let mut response =
        aggregate_stream_for_delivery(decoded, !request.include_native_response).await?;
    project_buffered_aggregate(
        &mut response,
        request.include_native_response,
        &requested_model,
    )?;
    let evidence = transport.observation.evidence();
    if evidence.snapshot.outcome == Some(crate::AttemptOutcome::ProviderRejected) {
        return Err(ProviderError::Transport(
            "Responses upstream reported failure".into(),
        ));
    }
    let usage = evidence
        .usage
        .filter(|usage| usage.completeness == UsageCompleteness::Final)
        .filter(|_| {
            evidence.snapshot.protocol_done
                && evidence.snapshot.qualification_error.is_none()
                && evidence.snapshot.outcome == Some(crate::AttemptOutcome::Success)
        })
        .ok_or(sandhi_core::usage::BufferedUsageError::UnqualifiedStream)
        .map(|mut usage| {
            usage.outcome = Some("success".into());
            usage
        });
    // Qualified setup owns exactly one physical response, including when its
    // final counters are absent. Decoder metadata is not attempt authority.
    Ok(crate::buffered_completion::observed(response, usage, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ByteStream, Provider, ProviderRequest, ProviderResponse, QualifiedProviderStream};
    use futures_util::StreamExt;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn incomplete_buffered_outcome_survives_both_views_and_invalid_usage() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        for native in [false, true] {
            for reason in ["max_output_tokens", "content_filter"] {
                for valid in [false, true] {
                    let server = MockServer::start().await;
                    let mut body = serde_json::json!({"id":"resp_fixture","model":"fixture",
                        "status":"incomplete","incomplete_details":{"reason":reason},"output":[],
                        "usage":{"input_tokens":5,"output_tokens":2}});
                    if !valid {
                        body["usage"]["input_tokens_details"] =
                            serde_json::json!({"cached_tokens":9});
                    }
                    let wire = format!(
                        "data: {}\n\n",
                        serde_json::json!({"type":"response.incomplete","response":body})
                    );
                    Mock::given(method("POST"))
                        .respond_with(
                            ResponseTemplate::new(200)
                                .insert_header("content-type", "text/event-stream")
                                .set_body_string(wire),
                        )
                        .mount(&server)
                        .await;
                    let provider = TypedOpenAiResponses::new(
                        "fixture".into(),
                        Arc::new(
                            crate::OpenAiResponses::new("fixture", server.uri(), "synthetic")
                                .with_profile(crate::OpenAiResponsesProfile::ChatGptCodex),
                        ),
                        crate::OpenAiResponsesProfile::ChatGptCodex,
                    );
                    let request: ChatRequestV1 = serde_json::from_value(serde_json::json!({
                        "model":"fixture","include_native_response":native,
                        "messages":[{"role":"developer","content":"Answer."},{"role":"user","content":"hello"}]
                    })).unwrap();
                    let result = complete(&provider, request, Default::default(), None)
                        .await
                        .unwrap();
                    assert_eq!(result.response.usage.outcome.as_deref(), Some("incomplete"));
                    assert_eq!(result.response.usage.attempts, 1);
                    assert_eq!(
                        result.response.finish_reason,
                        Some(if reason == "content_filter" {
                            sandhi_core::FinishReasonV1::ContentFilter
                        } else {
                            sandhi_core::FinishReasonV1::Length
                        })
                    );
                    if valid {
                        let usage = result.usage.unwrap();
                        assert_eq!(usage.outcome.as_deref(), Some("incomplete"));
                        assert_eq!((usage.tokens_in, usage.tokens_out), (5, 2));
                        assert_eq!(usage.completeness, UsageCompleteness::Final);
                    } else {
                        assert!(result.usage.is_err());
                        assert_eq!(
                            result.response.usage.completeness,
                            UsageCompleteness::Unavailable
                        );
                    }
                    assert_eq!(server.received_requests().await.unwrap().len(), 1);
                }
            }
        }
    }

    struct DecodeFailure {
        same_chunk: bool,
        polled: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Provider for DecodeFailure {
        fn slug(&self) -> &str {
            "fixture"
        }
        async fn complete(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            unreachable!()
        }
        async fn stream(&self, _: ProviderRequest) -> Result<ByteStream, ProviderError> {
            unreachable!()
        }
        async fn stream_qualified(
            &self,
            request: ProviderRequest,
            _: QualifiedStreamDelivery,
        ) -> Result<QualifiedProviderStream, ProviderError> {
            crate::attempt::observe_qualified_stream(request.attempt_context,"fixture",Some(&request.model),
                sandhi_core::EndpointFamilyV1::OpenaiResponses,Default::default(),Default::default(),|facts| async move {
                    facts.record_headers(200,&http::HeaderMap::new(),None);
                    let prefix="data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"in_progress\",\"output\":[]}}\n\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_0\",\"summary\":[],\"encrypted_content\":\"opaque\"}}\n\n";
                    let terminal="data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_fixture\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":4,\"output_tokens_details\":{\"reasoning_tokens\":0}}}}\n\n";
                    let chunks=if self.same_chunk {vec![format!("{prefix}{terminal}")]}else{vec![prefix.to_owned(),terminal.to_owned()]};
                    let polled=self.polled.clone();
                    let raw:crate::raw::RawChunkStream=Box::pin(futures_util::stream::iter(chunks).map(move|chunk|{
                        polled.fetch_add(1,Ordering::SeqCst);Ok(bytes::Bytes::from(chunk))
                    }));
                    Ok((raw,None))
                }).await
        }
    }

    #[tokio::test]
    async fn projection_refusal_drains_the_same_source_for_later_final_counts() {
        for same_chunk in [false, true] {
            let polled = Arc::new(AtomicUsize::new(0));
            let provider = TypedOpenAiResponses::new(
                "fixture".into(),
                Arc::new(DecodeFailure {
                    same_chunk,
                    polled: polled.clone(),
                }),
                crate::OpenAiResponsesProfile::ChatGptCodex,
            );
            let request:ChatRequestV1=serde_json::from_value(serde_json::json!({"model":"fixture","messages":[{"role":"developer","content":"Code"},{"role":"user","content":"hello"}]})).unwrap();
            let (context, receiver) =
                AttemptContext::single_attempt("execution", "d".repeat(64)).unwrap();
            assert!(complete(
                &provider,
                request,
                Default::default(),
                Some(context.clone())
            )
            .await
            .is_err());
            assert_eq!(
                polled.load(Ordering::SeqCst),
                if same_chunk { 1 } else { 2 }
            );
            let evidence = context.qualified_stream_evidence().unwrap();
            assert_eq!(
                evidence.snapshot.outcome,
                Some(crate::AttemptOutcome::Success)
            );
            let usage = evidence.usage.unwrap();
            assert_eq!(usage.tokens_out, 4);
            assert_eq!(usage.completeness, UsageCompleteness::Final);
            assert_eq!(usage.reasoning_tokens, Some(0));
            assert_eq!(receiver.drain().len(), 2);
        }
    }
}
