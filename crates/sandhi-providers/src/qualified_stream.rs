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
        while let Some(event)=inner.next().await {
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
        if evidence.snapshot.qualification_error.is_some()
            || !evidence.snapshot.protocol_done
            || evidence.snapshot.outcome!=Some(AttemptOutcome::Success)
            || !evidence.usage.as_ref().is_some_and(|usage| usage.completeness==UsageCompleteness::Final)
        {
            Err(ProviderError::Transport("upstream stream evidence is unqualified".into()))?;
        }
        yield ChatStreamEventV1::Usage{usage:evidence.usage.expect("qualified final source usage")};
        if let Some(reason)=finish { yield ChatStreamEventV1::Finish{reason}; }
    })
}
