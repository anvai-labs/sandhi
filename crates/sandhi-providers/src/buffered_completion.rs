//! One typed buffered usage projection for embedded and accounted consumers.
use crate::{ProviderError, QualifiedCompletion};
use sandhi_core::{usage::BufferedUsageError, ChatResponseV1, UsageCompleteness, UsageV2};

/// Pure decoding happens after the physical response arrived. Its structural
/// failures are deterministic output refusals, not retryable network failures.
pub(crate) fn decode_error(error: ProviderError) -> ProviderError {
    match error {
        error @ ProviderError::OutputProjection(_) => error,
        _ => ProviderError::OutputProjection(
            "upstream buffered response has invalid required output structure".into(),
        ),
    }
}

pub(crate) fn object_arguments(
    value: Option<&serde_json::Value>,
    optional: bool,
) -> Result<String, ProviderError> {
    match value {
        Some(value) if value.is_object() => Ok(value.to_string()),
        None | Some(serde_json::Value::Null) if optional => Ok("{}".into()),
        _ => Err(ProviderError::OutputProjection(
            "upstream function arguments require a JSON object".into(),
        )),
    }
}

pub(crate) fn unique_calls(calls: &[sandhi_core::ToolCallV1]) -> Result<(), ProviderError> {
    let mut ids = std::collections::BTreeSet::new();
    if calls.len() > 1024 || calls.iter().any(|call| !ids.insert(call.id.as_str())) {
        return Err(ProviderError::OutputProjection(
            "upstream tool call identities are duplicate or exceed the retention limit".into(),
        ));
    }
    Ok(())
}

pub(crate) fn observed(
    mut response: ChatResponseV1,
    usage: Result<UsageV2, BufferedUsageError>,
    attempts: u32,
) -> QualifiedCompletion<ChatResponseV1> {
    // Generation outcome and physical HTTP success are independent. Responses
    // decodes completed/incomplete/failed status into this metadata already.
    let outcome = response
        .usage
        .outcome
        .clone()
        .or_else(|| Some("success".into()));
    let usage = usage.map(|mut usage| {
        usage.attempts = attempts;
        usage.outcome.clone_from(&outcome);
        usage
    });
    response.usage = usage.as_ref().cloned().unwrap_or_else(|_| UsageV2 {
        // Availability is independent of valid numeric totals. Do not promote
        // legacy clamping/defaults into a final measurement on qualifier failure.
        cache_read_observation: response.usage.cache_read_observation,
        completeness: UsageCompleteness::Unavailable,
        attempts,
        outcome,
        duration_ms: response.usage.duration_ms,
        duration_source: response.usage.duration_source,
        time_to_first_token_ms: response.usage.time_to_first_token_ms,
        time_to_first_token_source: response.usage.time_to_first_token_source,
        ..Default::default()
    });
    QualifiedCompletion { response, usage }
}

impl QualifiedCompletion<ChatResponseV1> {
    pub(crate) fn into_response(self) -> Result<ChatResponseV1, ProviderError> {
        self.usage.map_err(|error| {
            ProviderError::OutputProjection(format!(
                "upstream buffered usage is unqualified ({error:?})"
            ))
        })?;
        Ok(self.response)
    }
}

pub(crate) fn qualify(
    response: &crate::ProviderResponse,
    family: sandhi_core::EndpointFamilyV1,
    default: sandhi_core::usage::AnthropicUsageProfile,
) -> Result<sandhi_core::UsageV2, sandhi_core::usage::BufferedUsageError> {
    sandhi_core::usage::qualify_buffered_usage_with_profile(
        family,
        response
            .usage_convention
            .map_or(default, |value| value.anthropic_profile()),
        &response.body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn qualified_counts_do_not_erase_response_generation_outcome() {
        for (status, expected) in [
            ("completed", "success"),
            ("incomplete", "incomplete"),
            ("failed", "error"),
            ("cancelled", "unknown"),
            ("unrecognized", "unknown"),
        ] {
            for valid in [true, false] {
                let body = json!({"status":status,"output":[],"usage":{"input_tokens":5,"output_tokens":2}});
                let response = crate::openai_responses_typed::decode_responses_response(
                    body.clone(),
                    Default::default(),
                    "fixture",
                )
                .unwrap();
                let usage = if valid {
                    sandhi_core::usage::qualify_buffered_usage(
                        sandhi_core::EndpointFamilyV1::OpenaiResponses,
                        &body,
                    )
                } else {
                    Err(BufferedUsageError::InvalidResponse)
                };
                let observed = observed(response, usage, 1);
                assert_eq!(observed.response.usage.outcome.as_deref(), Some(expected));
                assert_eq!(observed.response.usage.attempts, 1);
                if valid && status != "unrecognized" {
                    let usage = observed.usage.unwrap();
                    assert_eq!(usage.outcome.as_deref(), Some(expected));
                    assert_eq!((usage.tokens_in, usage.tokens_out), (5, 2));
                    assert_eq!(usage.completeness, UsageCompleteness::Final);
                } else {
                    assert!(observed.usage.is_err());
                    assert_eq!(
                        observed.response.usage.completeness,
                        UsageCompleteness::Unavailable
                    );
                    assert_eq!(
                        (
                            observed.response.usage.tokens_in,
                            observed.response.usage.tokens_out
                        ),
                        (0, 0)
                    );
                }
            }
        }
    }
}
