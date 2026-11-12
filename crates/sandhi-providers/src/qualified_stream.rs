//! Typed projection of strict source evidence. No transport, retry or settlement.
use crate::{raw::QualifiedStreamObservation, AttemptOutcome, ChatEventStream, ProviderError};
use futures_util::StreamExt;
use sandhi_core::{ChatStreamEventV1, UsageCompleteness, UsageV2};

pub(crate) fn decode(
    inner: ChatEventStream,
    observation: QualifiedStreamObservation,
) -> ChatEventStream {
    Box::pin(async_stream::try_stream! {
        let mut inner=inner;
        while let Some(event)=inner.next().await {
            match event? {
                ChatStreamEventV1::Usage{usage} if usage.completeness==UsageCompleteness::Final => {
                    let evidence=observation.evidence();
                    // Source counts, including explicit zero and completeness,
                    // replace the legacy decoder's permissive numeric projection.
                    let mut measured=evidence.usage.unwrap_or_else(||UsageV2{attempts:1,..Default::default()});
                    if evidence.snapshot.qualification_error.is_some() && measured.completeness==UsageCompleteness::Final {
                        measured.completeness=UsageCompleteness::Partial;
                    }
                    yield ChatStreamEventV1::Usage{usage:measured};
                }
                event=>yield event,
            }
        }
        drop(inner);
        let evidence=observation.evidence();
        if evidence.snapshot.qualification_error.is_some()
            || !evidence.snapshot.protocol_done
            || evidence.snapshot.outcome!=Some(AttemptOutcome::Success)
            || !evidence.usage.is_some_and(|usage| usage.completeness==UsageCompleteness::Final)
        {
            Err(ProviderError::Transport("upstream stream evidence is unqualified".into()))?;
        }
    })
}
