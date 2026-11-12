//! Buffered typed delivery over one qualified SSE transport response.
use super::{
    aggregate_stream, decode_responses_stream_delivery, provider_request_observed,
    TypedOpenAiResponses,
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
    let transport = provider
        .raw
        .stream_qualified(
            provider_request_observed(&request, body, headers, context),
            QualifiedStreamDelivery::Buffered,
        )
        .await?;
    let mut response = aggregate_stream(decode_responses_stream_delivery(
        transport.stream,
        request.model,
        request.include_native_response,
        true,
    ))
    .await?;
    if !request.include_native_response {
        response.extensions.clear();
    }
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
    if let Ok(usage) = &usage {
        response.usage = usage.clone();
    }
    Ok(QualifiedCompletion { response, usage })
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
                sandhi_core::EndpointFamilyV1::OpenaiResponses,Default::default(),|facts| async move {
                    facts.record_headers(200,&http::HeaderMap::new(),None);
                    let prefix="data: {\"type\":\"response.in_progress\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{}\"}\n\n";
                    let terminal="data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":4,\"output_tokens_details\":{\"reasoning_tokens\":0}}}}\n\n";
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
    async fn decoder_failure_retains_only_the_counts_actually_observed_by_the_source() {
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
            assert_eq!(polled.load(Ordering::SeqCst), 1);
            let evidence = context.qualified_stream_evidence().unwrap();
            assert_eq!(
                evidence.snapshot.outcome,
                Some(crate::AttemptOutcome::Cancelled)
            );
            let usage = evidence.usage.unwrap();
            assert_eq!(usage.tokens_out, if same_chunk { 4 } else { 1 });
            assert_eq!(
                usage.completeness,
                if same_chunk {
                    UsageCompleteness::Final
                } else {
                    UsageCompleteness::Partial
                }
            );
            assert_eq!(
                usage.reasoning_tokens,
                if same_chunk { Some(0) } else { None }
            );
            assert_eq!(receiver.drain().len(), 2);
        }
    }
}
