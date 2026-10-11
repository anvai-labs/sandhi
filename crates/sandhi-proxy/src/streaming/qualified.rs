//! Tracked streams use the existing raw transport, body owner and settlement jobs.
use super::*;
use crate::*;
use sandhi_providers::raw::QualifiedStreamObservation;
use sandhi_providers::AttemptOutcome;
use std::time::Instant;

pub(crate) struct OwnedObservation {
    observation: QualifiedStreamObservation,
    started: Instant,
    ttft_ms: Option<u64>,
    upstream_request_id: Option<String>,
}

/// Called after the source drops and before freezing the immutable terminal record.
/// Qualification failure retains the first counts as unresolved evidence, never zero.
pub(crate) fn capture_terminal(accounting: &mut RequestAccounting) {
    let Some(observed) = accounting.stream_observation.take() else {
        return;
    };
    let snapshot = observed.observation.snapshot();
    let mut usage: UsageV2 = snapshot.usage.map(Into::into).unwrap_or_default();
    usage.upstream_request_id = observed.upstream_request_id;
    reconcile_boundary_duration(&mut usage, elapsed_ms(observed.started));
    reconcile_boundary_ttft(&mut usage, observed.ttft_ms);
    if let Some(error) = snapshot.qualification_error {
        if snapshot.usage.is_some() {
            usage.completeness = UsageCompleteness::Partial;
        }
        // Bounded enum diagnostic, not a decision parsed from prose. The ledger's
        // existing completeness rule exclusively controls settlement eligibility.
        usage.outcome = Some(format!(
            "stream_qualification:{error:?};delivery:{}",
            accounting.outcome
        ));
        accounting.set_outcome("error");
    } else if snapshot.usage.is_none() {
        usage.outcome = Some(format!(
            "stream_usage_unavailable;delivery:{}",
            accounting.outcome
        ));
        accounting.set_outcome("error");
    } else {
        if accounting.outcome == "error" && snapshot.outcome == Some(AttemptOutcome::Timeout) {
            accounting.set_outcome("timeout");
        }
        usage.outcome = Some(accounting.outcome.into());
    }
    accounting.observe(&usage);
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn qualified_response(
    provider: ProviderHandle,
    request_body: Bytes,
    session: Option<String>,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
    full_error_detail: bool,
    permit: Arc<AdmissionPermit>,
) -> Response {
    let started = Instant::now();
    let forwarder = provider
        .raw_forwarder()
        .expect("tracked admission validates transport");
    let headers = accounting.per_call_wire_headers();
    let raw = match forwarder
        .forward_stream_qualified_with_headers(
            &upstream_path(provider.family(), None, dialect),
            request_body,
            session.as_deref(),
            Some(&accounting.request_id),
            &headers,
        )
        .await
    {
        Ok(raw) => raw,
        Err(error) => {
            accounting.set_outcome("error");
            if !accounting.finalize_owned().await {
                return settlement::buffered::accounting_error(dialect, &accounting.request_id);
            }
            return provider_error(&error, dialect, provider.slug(), full_error_detail);
        }
    };
    let mut upstream = raw.stream;
    let observation = raw.observation.clone();
    accounting.stream_observation = Some(OwnedObservation {
        observation: raw.observation,
        started,
        ttft_ms: None,
        upstream_request_id: raw.upstream_request_id,
    });
    let body = super::body(accounting, permit, None, move |accounting| {
        Box::pin(async_stream::stream! {
            while let Some(bytes) = upstream.next().await {
                match bytes {
                    Ok(bytes) => {
                        if !bytes.is_empty() {
                            accounting.stream_observation.as_mut().expect("response owner").ttft_ms.get_or_insert_with(|| elapsed_ms(started));
                        }
                        yield Ok(bytes);
                    }
                    Err(_) => {
                        yield Err(std::io::Error::other("upstream stream failed"));
                        return;
                    }
                }
            }
            let snapshot = observation.snapshot();
            if snapshot.outcome == Some(AttemptOutcome::Success) && snapshot.qualification_error.is_none() {
                accounting.set_outcome("success");
            } else {
                yield Err(std::io::Error::other("upstream stream qualification failed"));
            }
        })
    });
    let headers = sandhi_providers::raw::filter_response_headers(&raw.headers);
    let mut response = Response::builder().status(raw.status);
    for (name, value) in &headers {
        response = response.header(name, value);
    }
    if !headers.contains_key("content-type") {
        response = response.header("content-type", "text/event-stream");
    }
    response.body(body).expect("valid streaming response")
}
