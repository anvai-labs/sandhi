//! Tracked streams use the existing raw transport, body owner and settlement jobs.
use crate::{
    codec::IngressDialect, elapsed_ms, provider_error, reconcile_boundary_duration,
    reconcile_boundary_ttft, settlement, upstream_path, AdmissionPermit, RequestAccounting,
};
use axum::{body::Bytes, response::Response};
use futures_util::StreamExt;
use sandhi_core::{UsageCompleteness, UsageV2};
use sandhi_providers::raw::{
    QualifiedStreamEvidence, QualifiedStreamObservation, QualifiedStreamSnapshot,
};
use sandhi_providers::AttemptOutcome;
use sandhi_providers::ProviderHandle;
use std::sync::Arc;
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
    let evidence = observed.observation.evidence();
    let snapshot = evidence.snapshot;
    let has_usage = evidence.usage.is_some();
    let mut usage: UsageV2 = evidence.usage.unwrap_or_default();
    usage.upstream_request_id = observed.upstream_request_id;
    reconcile_boundary_duration(&mut usage, elapsed_ms(observed.started));
    reconcile_boundary_ttft(&mut usage, observed.ttft_ms);
    capture_usage(accounting, usage, has_usage, snapshot);
}

/// SSE-only buffered transports retain source evidence even when output
/// aggregation fails. This uses the same qualifier/completeness settlement rule.
pub(crate) fn capture_provider_terminal(
    accounting: &mut RequestAccounting,
    evidence: QualifiedStreamEvidence,
) {
    let has_usage = evidence.usage.is_some();
    let mut usage = evidence.usage.unwrap_or_else(|| UsageV2 {
        attempts: 1,
        ..Default::default()
    });
    if let Some(previous) = &accounting.usage {
        if usage.duration_ms.is_none() {
            usage.duration_ms = previous.duration_ms;
            usage.duration_source = previous.duration_source;
        }
        if usage.time_to_first_token_ms.is_none() {
            usage.time_to_first_token_ms = previous.time_to_first_token_ms;
            usage.time_to_first_token_source = previous.time_to_first_token_source;
        }
    }
    capture_usage(accounting, usage, has_usage, evidence.snapshot);
}

fn capture_usage(
    accounting: &mut RequestAccounting,
    mut usage: UsageV2,
    has_usage: bool,
    snapshot: QualifiedStreamSnapshot,
) {
    if let Some(error) = snapshot.qualification_error {
        if has_usage {
            usage.completeness = UsageCompleteness::Partial;
        }
        // Bounded enum diagnostic, not a decision parsed from prose. The ledger's
        // existing completeness rule exclusively controls settlement eligibility.
        usage.outcome = Some(format!(
            "stream_qualification:{error:?};delivery:{}",
            accounting.outcome
        ));
        accounting.set_outcome("error");
    } else if !has_usage {
        usage.outcome = Some(format!(
            "stream_usage_unavailable;delivery:{}",
            accounting.outcome
        ));
        accounting.set_outcome("error");
    } else {
        if snapshot.outcome == Some(AttemptOutcome::ProviderRejected) {
            accounting.set_outcome("error");
        }
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
    gemini: Option<crate::GeminiRoute>,
    permit: Arc<AdmissionPermit>,
) -> Response {
    let started = Instant::now();
    let forwarder = provider
        .raw_forwarder()
        .expect("tracked admission validates transport")
        .clone()
        .with_metered_attempt_context(
            provider.slug(),
            Some(accounting.model.clone()),
            accounting
                .owned
                .as_ref()
                .expect("tracked owner")
                .attempts
                .context
                .clone(),
        );
    let headers = accounting.per_call_wire_headers();
    let raw = match forwarder
        .forward_stream_qualified_with_headers(
            &upstream_path(provider.family(), gemini.as_ref(), dialect),
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
    crate::stamp_usage_convention(
        response.body(body).expect("valid streaming response"),
        &provider,
        dialect,
        true,
    )
}
