//! Strict qualification around the existing family parsers. No I/O or estimates.
use super::{BufferedUsageError as Error, MAX_PLAUSIBLE_TOKENS};
use crate::{EndpointFamilyV1, UsageV2};
use serde_json::Value;

/// Explicit endpoint convention, independent of request cache-control capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AnthropicUsageProfile {
    #[default]
    SeparateCacheWrites,
    /// Uncached input includes implicit cache construction; no separate write
    /// category is metered. Missing/zero writes are allowed, nonzero writes refuse.
    ImplicitCache,
}

fn required(object: &Value, key: &str, field: &'static str) -> Result<u64, Error> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n <= MAX_PLAUSIBLE_TOKENS)
        .ok_or(Error::InvalidField(field))
}
fn optional(object: &Value, key: &str, field: &'static str) -> Result<Option<u64>, Error> {
    object
        .get(key)
        .map(|_| required(object, key, field))
        .transpose()
}
fn details<'a>(object: &'a Value, key: &'static str) -> Result<Option<&'a Value>, Error> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) if value.is_object() => Ok(Some(value)),
        _ => Err(Error::InvalidField(key)),
    }
}
fn detail(
    object: &Value,
    key: &'static str,
    counter: &str,
    field: &'static str,
) -> Result<Option<u64>, Error> {
    details(object, key)?
        .map(|d| optional(d, counter, field))
        .transpose()
        .map(Option::flatten)
}
fn total(object: &Value, key: &str, expected: u64, field: &'static str) -> Result<(), Error> {
    if optional(object, key, field)?.is_some_and(|n| n != expected) {
        return Err(Error::InconsistentUsage);
    }
    Ok(())
}
fn usage<'a>(response: &'a Value, key: &'static str) -> Result<&'a Value, Error> {
    if !response.is_object() {
        return Err(Error::InvalidResponse);
    }
    let value = response.get(key).ok_or(Error::MissingUsage)?;
    if !value.is_object() {
        return Err(Error::InvalidField(key));
    }
    Ok(value)
}

fn responses(response: &Value) -> Result<(), Error> {
    let usage = usage(response, "usage")?;
    if response.get("status").is_some_and(|s| {
        !matches!(
            s.as_str(),
            Some("completed" | "incomplete" | "failed" | "cancelled")
        )
    }) {
        return Err(Error::InvalidField("status"));
    }
    let input = required(usage, "input_tokens", "input_tokens")?;
    let output = required(usage, "output_tokens", "output_tokens")?;
    let cached = detail(
        usage,
        "input_tokens_details",
        "cached_tokens",
        "input_tokens_details.cached_tokens",
    )?
    .unwrap_or(0);
    let written = detail(
        usage,
        "input_tokens_details",
        "cache_write_tokens",
        "input_tokens_details.cache_write_tokens",
    )?
    .unwrap_or(0);
    let reasoning = detail(
        usage,
        "output_tokens_details",
        "reasoning_tokens",
        "output_tokens_details.reasoning_tokens",
    )?
    .unwrap_or(0);
    if cached + written > input || reasoning > output {
        return Err(Error::InconsistentUsage);
    }
    total(usage, "total_tokens", input + output, "total_tokens")
}

/// Validate reported progress counters without declaring request-total usage.
/// Missing categories contribute no observed quantity; known cached/reasoning
/// subsets establish only an inclusive lower bound. This can never settle a lease.
pub fn qualify_responses_progress_usage(response: &Value) -> Result<Option<UsageV2>, Error> {
    if !response.is_object() {
        return Err(Error::InvalidResponse);
    }
    match response.get("usage") {
        None | Some(Value::Null) => return Ok(None),
        Some(value) if value.is_object() => {}
        _ => return Err(Error::InvalidField("usage")),
    }
    let counters = &response["usage"];
    let input = optional(counters, "input_tokens", "input_tokens")?;
    let output = optional(counters, "output_tokens", "output_tokens")?;
    let cached = detail(
        counters,
        "input_tokens_details",
        "cached_tokens",
        "input_tokens_details.cached_tokens",
    )?;
    let written = detail(
        counters,
        "input_tokens_details",
        "cache_write_tokens",
        "input_tokens_details.cache_write_tokens",
    )?;
    let reasoning = detail(
        counters,
        "output_tokens_details",
        "reasoning_tokens",
        "output_tokens_details.reasoning_tokens",
    )?;
    let total = optional(counters, "total_tokens", "total_tokens")?;
    let known_input = cached.unwrap_or(0) + written.unwrap_or(0);
    let inclusive_input = input.unwrap_or(known_input);
    let inclusive_output = output.unwrap_or(reasoning.unwrap_or(0));
    if known_input > inclusive_input
        || reasoning.is_some_and(|n| n > inclusive_output)
        || total.is_some_and(|n| n < inclusive_input + inclusive_output)
        || total
            .zip(input)
            .zip(output)
            .is_some_and(|((total, input), output)| total != input + output)
    {
        return Err(Error::InconsistentUsage);
    }
    if [input, output, cached, written, reasoning]
        .iter()
        .all(Option::is_none)
    {
        return Ok(None);
    }
    Ok(Some(UsageV2 {
        tokens_in: inclusive_input - known_input,
        tokens_out: inclusive_output,
        cache_read_tokens: cached.unwrap_or(0),
        cache_creation_tokens: written.unwrap_or(0),
        cache_read_observation: Some(crate::observe_cache_read(
            crate::CacheReadFamily::OpenAiResponses,
            response,
        )),
        reasoning_tokens: reasoning,
        reasoning_included: Some(true),
        completeness: crate::UsageCompleteness::Partial,
        attempts: 1,
        ..Default::default()
    }))
}

/// Validate the counters present in one cumulative Messages progress update.
/// Missing categories remain unknown. Cache-write TTL subsets and thinking can
/// establish lower bounds, but this result is always partial and cannot settle.
pub fn qualify_anthropic_progress_usage(
    response: &Value,
    profile: AnthropicUsageProfile,
) -> Result<Option<UsageV2>, Error> {
    if !response.is_object() {
        return Err(Error::InvalidResponse);
    }
    match response.get("usage") {
        None | Some(Value::Null) => return Ok(None),
        Some(value) if value.is_object() => {}
        _ => return Err(Error::InvalidField("usage")),
    }
    let counters = &response["usage"];
    let input = optional(counters, "input_tokens", "input_tokens")?;
    let output = optional(counters, "output_tokens", "output_tokens")?;
    let cached = optional(
        counters,
        "cache_read_input_tokens",
        "cache_read_input_tokens",
    )?;
    let written = optional(
        counters,
        "cache_creation_input_tokens",
        "cache_creation_input_tokens",
    )?;
    let reasoning = detail(
        counters,
        "output_tokens_details",
        "thinking_tokens",
        "output_tokens_details.thinking_tokens",
    )?;
    let short = detail(
        counters,
        "cache_creation",
        "ephemeral_5m_input_tokens",
        "cache_creation.ephemeral_5m_input_tokens",
    )?;
    let long = detail(
        counters,
        "cache_creation",
        "ephemeral_1h_input_tokens",
        "cache_creation.ephemeral_1h_input_tokens",
    )?;
    let split = short.unwrap_or(0) + long.unwrap_or(0);
    if written.is_some_and(|n| split > n)
        || written
            .zip(short)
            .zip(long)
            .is_some_and(|((n, a), b)| n != a + b)
        || output.zip(reasoning).is_some_and(|(n, r)| r > n)
        || (profile == AnthropicUsageProfile::ImplicitCache
            && (written.unwrap_or(0) != 0 || split != 0))
    {
        return Err(Error::InconsistentUsage);
    }
    if [input, output, cached, written, reasoning, short, long]
        .iter()
        .all(Option::is_none)
    {
        return Ok(None);
    }
    Ok(Some(UsageV2 {
        tokens_in: input.unwrap_or(0),
        tokens_out: output.unwrap_or(reasoning.unwrap_or(0)),
        cache_read_tokens: cached.unwrap_or(0),
        cache_creation_tokens: written.unwrap_or(split),
        cache_read_observation: Some(crate::observe_cache_read(
            crate::CacheReadFamily::Anthropic,
            response,
        )),
        reasoning_tokens: reasoning,
        reasoning_included: Some(true),
        completeness: crate::UsageCompleteness::Partial,
        attempts: 1,
        ..Default::default()
    }))
}

fn anthropic(response: &Value, profile: AnthropicUsageProfile) -> Result<(), Error> {
    let usage = usage(response, "usage")?;
    required(usage, "input_tokens", "input_tokens")?;
    let output = required(usage, "output_tokens", "output_tokens")?;
    // Input is exclusive of caching: unlike inclusive Chat/Responses input,
    // missing cache counters cannot be safely charged as fresh input instead.
    let written = match profile {
        AnthropicUsageProfile::SeparateCacheWrites => required(
            usage,
            "cache_creation_input_tokens",
            "cache_creation_input_tokens",
        )?,
        AnthropicUsageProfile::ImplicitCache => {
            let written = optional(
                usage,
                "cache_creation_input_tokens",
                "cache_creation_input_tokens",
            )?
            .unwrap_or(0);
            if written != 0 {
                return Err(Error::InconsistentUsage);
            }
            written
        }
    };
    required(usage, "cache_read_input_tokens", "cache_read_input_tokens")?;
    let reasoning = detail(
        usage,
        "output_tokens_details",
        "thinking_tokens",
        "output_tokens_details.thinking_tokens",
    )?
    .unwrap_or(0);
    if reasoning > output {
        return Err(Error::InconsistentUsage);
    }
    if let Some(split) = details(usage, "cache_creation")? {
        let short = optional(
            split,
            "ephemeral_5m_input_tokens",
            "cache_creation.ephemeral_5m_input_tokens",
        )?;
        let long = optional(
            split,
            "ephemeral_1h_input_tokens",
            "cache_creation.ephemeral_1h_input_tokens",
        )?;
        if short.unwrap_or(0) + long.unwrap_or(0) > written
            || short.zip(long).is_some_and(|(a, b)| a + b != written)
        {
            return Err(Error::InconsistentUsage);
        }
    }
    Ok(())
}

fn gemini(response: &Value) -> Result<(), Error> {
    let usage = usage(response, "usageMetadata")?;
    let prompt = required(usage, "promptTokenCount", "promptTokenCount")?;
    let output = required(usage, "candidatesTokenCount", "candidatesTokenCount")?;
    let cached =
        optional(usage, "cachedContentTokenCount", "cachedContentTokenCount")?.unwrap_or(0);
    let reasoning = optional(usage, "thoughtsTokenCount", "thoughtsTokenCount")?.unwrap_or(0);
    let tools = optional(usage, "toolUsePromptTokenCount", "toolUsePromptTokenCount")?.unwrap_or(0);
    if cached > prompt || prompt + tools > MAX_PLAUSIBLE_TOKENS {
        return Err(Error::InconsistentUsage);
    }
    total(
        usage,
        "totalTokenCount",
        prompt + tools + output + reasoning,
        "totalTokenCount",
    )
}

/// Qualify only the Gemini counters actually reported so far. Cached input is a
/// subset of prompt input; tool-result input and thoughts are separate counts.
/// Neither a complete usage object nor HTTP EOF alone proves generation ended.
pub fn qualify_gemini_progress_usage(response: &Value) -> Result<Option<UsageV2>, Error> {
    if !response.is_object() {
        return Err(Error::InvalidResponse);
    }
    let counters = match response.get("usageMetadata") {
        None | Some(Value::Null) => return Ok(None),
        Some(value) if value.is_object() => value,
        _ => return Err(Error::InvalidField("usageMetadata")),
    };
    let prompt = optional(counters, "promptTokenCount", "promptTokenCount")?;
    let output = optional(counters, "candidatesTokenCount", "candidatesTokenCount")?;
    let cached = optional(
        counters,
        "cachedContentTokenCount",
        "cachedContentTokenCount",
    )?;
    let reasoning = optional(counters, "thoughtsTokenCount", "thoughtsTokenCount")?;
    let tools = optional(
        counters,
        "toolUsePromptTokenCount",
        "toolUsePromptTokenCount",
    )?;
    let total = optional(counters, "totalTokenCount", "totalTokenCount")?;
    let inclusive_prompt = prompt.unwrap_or(cached.unwrap_or(0));
    let known_total =
        inclusive_prompt + tools.unwrap_or(0) + output.unwrap_or(0) + reasoning.unwrap_or(0);
    if cached.is_some_and(|n| n > inclusive_prompt)
        || inclusive_prompt + tools.unwrap_or(0) > MAX_PLAUSIBLE_TOKENS
        || total.is_some_and(|n| n < known_total)
        || total
            .zip(prompt)
            .zip(output)
            .is_some_and(|((n, _), _)| n != known_total)
    {
        return Err(Error::InconsistentUsage);
    }
    if [prompt, output, cached, reasoning, tools]
        .iter()
        .all(Option::is_none)
    {
        return Ok(None);
    }
    Ok(Some(UsageV2 {
        tokens_in: inclusive_prompt - cached.unwrap_or(0) + tools.unwrap_or(0),
        tokens_out: output.unwrap_or(0),
        cache_read_tokens: cached.unwrap_or(0),
        cache_read_observation: Some(crate::observe_cache_read(
            crate::CacheReadFamily::Gemini,
            response,
        )),
        reasoning_tokens: reasoning,
        reasoning_included: Some(false),
        completeness: crate::UsageCompleteness::Partial,
        attempts: 1,
        ..Default::default()
    }))
}

/// Qualify provider-reported buffered usage for the selected wire protocol.
/// Keeps the existing parser's count derivation, preserves reported reasoning zero,
/// and refuses missing/malformed/inconsistent required evidence. Optional inclusive
/// cache details retain their reporting-coverage metadata; this is not hit proof.
/// Anthropic requires all four disjoint counters to avoid undercounting omitted cache
/// categories. HTTP completion, physical send and settlement remain separate facts.
pub fn qualify_buffered_usage(
    endpoint: EndpointFamilyV1,
    response: &Value,
) -> Result<UsageV2, Error> {
    qualify_buffered_usage_with_profile(
        endpoint,
        AnthropicUsageProfile::SeparateCacheWrites,
        response,
    )
}

pub fn qualify_buffered_usage_with_profile(
    endpoint: EndpointFamilyV1,
    profile: AnthropicUsageProfile,
    response: &Value,
) -> Result<UsageV2, Error> {
    if profile != AnthropicUsageProfile::SeparateCacheWrites
        && endpoint != EndpointFamilyV1::AnthropicMessages
    {
        return Err(Error::UnsupportedEndpoint);
    }
    let (parsed, reasoning_path) = match endpoint {
        EndpointFamilyV1::OpenaiChatCompletions => {
            super::validate_openai_buffered_usage(response)?;
            (
                super::parse_openai_usage(response),
                "/usage/completion_tokens_details/reasoning_tokens",
            )
        }
        EndpointFamilyV1::OpenaiResponses => {
            responses(response)?;
            (
                super::parse_openai_responses_usage(response),
                "/usage/output_tokens_details/reasoning_tokens",
            )
        }
        EndpointFamilyV1::AnthropicMessages => {
            anthropic(response, profile)?;
            (
                super::parse_anthropic_usage(response),
                "/usage/output_tokens_details/thinking_tokens",
            )
        }
        EndpointFamilyV1::GeminiGenerateContent => {
            gemini(response)?;
            (
                super::parse_gemini_usage(response),
                "/usageMetadata/thoughtsTokenCount",
            )
        }
        _ => return Err(Error::UnsupportedEndpoint),
    };
    let mut usage: UsageV2 = parsed.ok_or(Error::InvalidResponse)?.into();
    usage.reasoning_tokens = response.pointer(reasoning_path).and_then(Value::as_u64);
    if endpoint == EndpointFamilyV1::OpenaiChatCompletions {
        // These optional details were validated against their inclusive parents.
        // Keep reported zero distinct from absence; they are subsets, not extra charges.
        usage.audio_input_tokens = response
            .pointer("/usage/prompt_tokens_details/audio_tokens")
            .and_then(Value::as_u64);
        usage.audio_output_tokens = response
            .pointer("/usage/completion_tokens_details/audio_tokens")
            .and_then(Value::as_u64);
        usage.accepted_prediction_tokens = response
            .pointer("/usage/completion_tokens_details/accepted_prediction_tokens")
            .and_then(Value::as_u64);
        usage.rejected_prediction_tokens = response
            .pointer("/usage/completion_tokens_details/rejected_prediction_tokens")
            .and_then(Value::as_u64);
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_qualified_usage_preserves_optional_detail_subsets_and_reported_zero() {
        let mut body = json!({"usage":{"prompt_tokens":11,"completion_tokens":7,
            "prompt_tokens_details":{"cached_tokens":4,"cache_write_tokens":1,"audio_tokens":2},
            "completion_tokens_details":{"reasoning_tokens":1,"audio_tokens":2,"accepted_prediction_tokens":1,"rejected_prediction_tokens":2}}});
        let qualified =
            |body: &Value| qualify_buffered_usage(EndpointFamilyV1::OpenaiChatCompletions, body);
        let usage = qualified(&body).unwrap();
        assert_eq!(
            (
                usage.tokens_in,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.tokens_out
            ),
            (6, 4, 1, 7)
        );
        assert_eq!(
            (
                usage.audio_input_tokens,
                usage.audio_output_tokens,
                usage.accepted_prediction_tokens,
                usage.rejected_prediction_tokens
            ),
            (Some(2), Some(2), Some(1), Some(2))
        );
        body["usage"]["completion_tokens_details"]["accepted_prediction_tokens"] = 6.into();
        assert_eq!(qualified(&body), Err(Error::InconsistentUsage));
        body["usage"]["completion_tokens_details"]["accepted_prediction_tokens"] = 0.into();
        assert_eq!(
            qualified(&body).unwrap().accepted_prediction_tokens,
            Some(0)
        );
        body["usage"]["completion_tokens_details"]
            .as_object_mut()
            .unwrap()
            .remove("accepted_prediction_tokens");
        assert_eq!(qualified(&body).unwrap().accepted_prediction_tokens, None);
        body["usage"]["prompt_tokens_details"]["audio_tokens"] = 12.into();
        assert_eq!(qualified(&body), Err(Error::InconsistentUsage));
    }

    #[test]
    fn gemini_progress_keeps_exclusive_thoughts_and_tool_input_and_cache_subset() {
        for response in [
            json!({}),
            json!({"usageMetadata":null}),
            json!({"usageMetadata":{}}),
            json!({"usageMetadata":{"totalTokenCount":20}}),
        ] {
            assert_eq!(qualify_gemini_progress_usage(&response).unwrap(), None);
        }
        let usage = qualify_gemini_progress_usage(&json!({"usageMetadata":{
            "promptTokenCount":11,"cachedContentTokenCount":8,"toolUsePromptTokenCount":1,
            "candidatesTokenCount":4,"thoughtsTokenCount":2,"totalTokenCount":18
        }}))
        .unwrap()
        .unwrap();
        assert_eq!(
            (usage.tokens_in, usage.cache_read_tokens, usage.tokens_out),
            (4, 8, 4)
        );
        assert_eq!(usage.reasoning_tokens, Some(2));
        assert_eq!(usage.reasoning_included, Some(false));
        assert_eq!(usage.completeness, crate::UsageCompleteness::Partial);
        let subset = qualify_gemini_progress_usage(&json!({"usageMetadata":{
            "cachedContentTokenCount":8,"thoughtsTokenCount":5
        }}))
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                subset.tokens_in,
                subset.cache_read_tokens,
                subset.tokens_out
            ),
            (0, 8, 0)
        );
        assert_eq!(
            subset.reasoning_tokens,
            Some(5),
            "thoughts need no output parent"
        );
        for reasoning in [None, Some(0)] {
            let mut value =
                json!({"usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":0}});
            if let Some(n) = reasoning {
                value["usageMetadata"]["thoughtsTokenCount"] = n.into();
            }
            assert_eq!(
                qualify_gemini_progress_usage(&value)
                    .unwrap()
                    .unwrap()
                    .reasoning_tokens,
                reasoning
            );
        }
    }

    #[test]
    fn gemini_progress_rejects_bad_counters_and_contradictory_totals() {
        for key in [
            "promptTokenCount",
            "candidatesTokenCount",
            "cachedContentTokenCount",
            "thoughtsTokenCount",
            "toolUsePromptTokenCount",
            "totalTokenCount",
        ] {
            for bad in [
                Value::Null,
                json!(-1),
                json!(1.5),
                json!("1"),
                json!(50_000_000_001_u64),
            ] {
                assert!(
                    qualify_gemini_progress_usage(&json!({"usageMetadata":{key:bad}})).is_err()
                );
            }
        }
        for value in [
            json!([]),
            json!({"usageMetadata":[]}),
            json!({"usageMetadata":{
                "promptTokenCount":3,"cachedContentTokenCount":4
            }}),
            json!({"usageMetadata":{"thoughtsTokenCount":5,"totalTokenCount":4}}),
            json!({"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,
            "thoughtsTokenCount":2,"toolUsePromptTokenCount":1,"totalTokenCount":9}}),
            json!({"usageMetadata":{"promptTokenCount":50_000_000_000_u64,"toolUsePromptTokenCount":1}}),
        ] {
            assert!(qualify_gemini_progress_usage(&value).is_err(), "{value}");
        }
    }

    #[test]
    fn anthropic_progress_preserves_unknown_zero_and_exclusive_cache_categories() {
        for response in [json!({}), json!({"usage":null}), json!({"usage":{}})] {
            assert_eq!(
                qualify_anthropic_progress_usage(&response, Default::default()).unwrap(),
                None
            );
        }
        let usage = qualify_anthropic_progress_usage(
            &json!({"usage":{
                "input_tokens":3,"cache_read_input_tokens":8,"cache_creation_input_tokens":2,
                "output_tokens":4,"output_tokens_details":{"thinking_tokens":0}
            }}),
            Default::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                usage.tokens_in,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.tokens_out
            ),
            (3, 8, 2, 4)
        );
        assert_eq!(usage.reasoning_tokens, Some(0));
        assert_eq!(usage.reasoning_included, Some(true));
        assert_eq!(usage.completeness, crate::UsageCompleteness::Partial);
        assert_eq!(
            usage.cache_read_observation.unwrap().status,
            crate::CacheReadStatus::Reported
        );
        let zero = qualify_anthropic_progress_usage(
            &json!({"usage":{"output_tokens":0}}),
            Default::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(zero.reasoning_tokens, None);
        assert_eq!(
            zero.cache_read_observation.unwrap().status,
            crate::CacheReadStatus::Absent
        );
        assert_eq!(zero.completeness, crate::UsageCompleteness::Partial);
    }

    #[test]
    fn anthropic_progress_subsets_establish_only_partial_lower_bounds() {
        let usage = qualify_anthropic_progress_usage(
            &json!({"usage":{
                "cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":3},
                "output_tokens_details":{"thinking_tokens":4}
            }}),
            Default::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                usage.tokens_in,
                usage.cache_creation_tokens,
                usage.tokens_out
            ),
            (0, 5, 4)
        );
        assert_eq!(usage.reasoning_tokens, Some(4));
        assert_eq!(usage.completeness, crate::UsageCompleteness::Partial);
        for counters in [
            json!({"output_tokens":3,"output_tokens_details":{"thinking_tokens":4}}),
            json!({"cache_creation_input_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":2}}),
            json!({"cache_creation_input_tokens":4,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":1}}),
        ] {
            assert!(qualify_anthropic_progress_usage(
                &json!({"usage":counters}),
                Default::default()
            )
            .is_err());
        }
    }

    #[test]
    fn anthropic_progress_refuses_malformed_known_counters_and_wrong_implicit_writes() {
        for path in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            for invalid in [
                Value::Null,
                json!(-1),
                json!(1.5),
                json!("1"),
                json!(50_000_000_001_u64),
            ] {
                assert!(qualify_anthropic_progress_usage(
                    &json!({"usage":{path:invalid}}),
                    Default::default()
                )
                .is_err());
            }
        }
        for counters in [
            json!({"cache_creation":[]}),
            json!({"output_tokens_details":false}),
            json!({"cache_creation":{"ephemeral_1h_input_tokens":null}}),
            json!({"output_tokens_details":{"thinking_tokens":"1"}}),
        ] {
            assert!(qualify_anthropic_progress_usage(
                &json!({"usage":counters}),
                Default::default()
            )
            .is_err());
        }
        for counts in [
            json!({"cache_creation_input_tokens":1}),
            json!({"cache_creation":{"ephemeral_5m_input_tokens":1}}),
        ] {
            assert!(qualify_anthropic_progress_usage(
                &json!({"usage":counts}),
                AnthropicUsageProfile::ImplicitCache
            )
            .is_err());
        }
        let zero = qualify_anthropic_progress_usage(
            &json!({"usage":{"cache_creation_input_tokens":0}}),
            AnthropicUsageProfile::ImplicitCache,
        )
        .unwrap()
        .unwrap();
        assert_eq!(zero.completeness, crate::UsageCompleteness::Partial);
    }

    #[test]
    fn responses_progress_is_partial_and_preserves_unknown_and_zero() {
        for response in [
            json!({}),
            json!({"usage":null}),
            json!({"usage":{}}),
            json!({"usage":{"total_tokens":10}}),
        ] {
            assert_eq!(qualify_responses_progress_usage(&response).unwrap(), None);
        }
        let usage = qualify_responses_progress_usage(&json!({"usage":{
            "input_tokens_details":{"cached_tokens":8,"cache_write_tokens":2},
            "output_tokens_details":{"reasoning_tokens":3}
        }}))
        .unwrap()
        .unwrap();
        assert_eq!(
            (
                usage.tokens_in,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.tokens_out
            ),
            (0, 8, 2, 3)
        );
        assert_eq!(usage.reasoning_tokens, Some(3));
        assert_eq!(usage.completeness, crate::UsageCompleteness::Partial);
        let zero = qualify_responses_progress_usage(
            &json!({"usage":{"input_tokens":0,"output_tokens_details":{"reasoning_tokens":0}}}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(zero.reasoning_tokens, Some(0));
        assert_eq!(zero.completeness, crate::UsageCompleteness::Partial);
        for response in [
            json!({"usage":false}),
            json!({"usage":{"input_tokens":null}}),
            json!({"usage":{"output_tokens":-1}}),
            json!({"usage":{"input_tokens":5,"input_tokens_details":{"cached_tokens":6}}}),
            json!({"usage":{"output_tokens":2,"output_tokens_details":{"reasoning_tokens":3}}}),
            json!({"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":8}}),
            json!({"usage":{"input_tokens_details":{"cached_tokens":5},"total_tokens":4}}),
            json!({"usage":{"output_tokens_details":{"reasoning_tokens":"0"}}}),
        ] {
            assert!(
                qualify_responses_progress_usage(&response).is_err(),
                "{response}"
            );
        }
    }

    #[test]
    fn implicit_messages_profile_is_explicit_and_never_hides_write_counters() {
        let family = EndpointFamilyV1::AnthropicMessages;
        let mut response =
            json!({"usage":{"input_tokens":5,"output_tokens":20,"cache_read_input_tokens":100}});
        assert!(qualify_buffered_usage(family, &response).is_err());
        let usage = qualify_buffered_usage_with_profile(
            family,
            AnthropicUsageProfile::ImplicitCache,
            &response,
        )
        .unwrap();
        assert_eq!(
            (
                usage.tokens_in,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.tokens_out
            ),
            (5, 100, 0, 20)
        );
        for written in [json!(null), json!(1), json!("0")] {
            response["usage"]["cache_creation_input_tokens"] = written;
            assert!(qualify_buffered_usage_with_profile(
                family,
                AnthropicUsageProfile::ImplicitCache,
                &response
            )
            .is_err());
        }
        response["usage"]["cache_creation_input_tokens"] = json!(0);
        assert!(qualify_buffered_usage_with_profile(
            family,
            AnthropicUsageProfile::ImplicitCache,
            &response
        )
        .is_ok());
        assert_eq!(
            qualify_buffered_usage_with_profile(
                EndpointFamilyV1::OpenaiResponses,
                AnthropicUsageProfile::ImplicitCache,
                &response
            ),
            Err(Error::UnsupportedEndpoint)
        );
        response["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_read_input_tokens");
        assert!(qualify_buffered_usage_with_profile(
            family,
            AnthropicUsageProfile::ImplicitCache,
            &response
        )
        .is_err());
    }

    #[test]
    fn family_qualification_conserves_input_output_and_reasoning() {
        for (family, response, counts, billable) in [
            (
                EndpointFamilyV1::OpenaiResponses,
                json!({"status":"incomplete","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,
                "input_tokens_details":{"cached_tokens":60,"cache_write_tokens":30},"output_tokens_details":{"reasoning_tokens":10}}}),
                (10, 60, 30, 20, Some(10), Some(true)),
                120,
            ),
            (
                EndpointFamilyV1::AnthropicMessages,
                json!({"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":60,"cache_creation_input_tokens":30,
                "cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20},"output_tokens_details":{"thinking_tokens":10}}}),
                (10, 60, 30, 20, Some(10), Some(true)),
                120,
            ),
            (
                EndpointFamilyV1::GeminiGenerateContent,
                json!({"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":60,"candidatesTokenCount":20,
                "thoughtsTokenCount":10,"toolUsePromptTokenCount":5,"totalTokenCount":135}}),
                (45, 60, 0, 20, Some(10), Some(false)),
                135,
            ),
        ] {
            let usage = qualify_buffered_usage(family, &response).unwrap();
            assert_eq!(
                (
                    usage.tokens_in,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens,
                    usage.tokens_out,
                    usage.reasoning_tokens,
                    usage.reasoning_included
                ),
                counts
            );
            assert_eq!(crate::billable(&usage), billable);
        }
    }

    #[test]
    fn family_qualification_distinguishes_reported_zero_missing_and_malformed() {
        for (family, mut response, root, required, reasoning) in [
            (
                EndpointFamilyV1::OpenaiResponses,
                json!({"usage":{"input_tokens":0,"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0}}}),
                "usage",
                "input_tokens",
                "/usage/output_tokens_details/reasoning_tokens",
            ),
            (
                EndpointFamilyV1::AnthropicMessages,
                json!({"usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens_details":{"thinking_tokens":0}}}),
                "usage",
                "cache_read_input_tokens",
                "/usage/output_tokens_details/thinking_tokens",
            ),
            (
                EndpointFamilyV1::GeminiGenerateContent,
                json!({"usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":0,"thoughtsTokenCount":0}}),
                "usageMetadata",
                "candidatesTokenCount",
                "/usageMetadata/thoughtsTokenCount",
            ),
        ] {
            let measured = qualify_buffered_usage(family, &response).unwrap();
            assert_eq!(crate::billable(&measured), 0);
            assert_eq!(measured.reasoning_tokens, Some(0));
            assert_eq!(measured.completeness, crate::UsageCompleteness::Final);
            assert_eq!(
                qualify_buffered_usage(family, &json!({})),
                Err(Error::MissingUsage)
            );
            let mut absent = response.clone();
            absent[root].as_object_mut().unwrap().remove(required);
            assert!(qualify_buffered_usage(family, &absent).is_err());
            for value in [
                json!(null),
                json!(-1),
                json!(1.5),
                json!("1"),
                json!(MAX_PLAUSIBLE_TOKENS + 1),
            ] {
                let mut bad = response.clone();
                bad[root][required] = value;
                assert!(qualify_buffered_usage(family, &bad).is_err());
            }
            *response.pointer_mut(reasoning).unwrap() = json!(1);
            if family != EndpointFamilyV1::GeminiGenerateContent {
                assert_eq!(
                    qualify_buffered_usage(family, &response),
                    Err(Error::InconsistentUsage)
                );
            }
        }
    }

    #[test]
    fn family_qualification_rejects_invalid_splits_totals_details_and_pending_responses() {
        for (family, responses) in [
            (
                EndpointFamilyV1::OpenaiResponses,
                vec![
                    json!({"usage":{"input_tokens":5,"output_tokens":2,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":2}}}),
                    json!({"status":"in_progress","usage":{"input_tokens":5,"output_tokens":2}}),
                    json!({"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":8}}),
                    json!({"usage":{"input_tokens":5,"output_tokens":2,"input_tokens_details":[]}}),
                ],
            ),
            (
                EndpointFamilyV1::AnthropicMessages,
                vec![
                    json!({"usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":2}}}),
                    json!({"usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":2,"output_tokens_details":{"thinking_tokens":"1"}}}),
                ],
            ),
            (
                EndpointFamilyV1::GeminiGenerateContent,
                vec![
                    json!({"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"cachedContentTokenCount":6}}),
                    json!({"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"thoughtsTokenCount":2,"totalTokenCount":7}}),
                    json!({"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"toolUsePromptTokenCount":null}}),
                ],
            ),
        ] {
            for response in responses {
                assert!(
                    qualify_buffered_usage(family, &response).is_err(),
                    "{response}"
                );
            }
        }
        assert_eq!(
            qualify_buffered_usage(EndpointFamilyV1::HostCallback, &json!({})),
            Err(Error::UnsupportedEndpoint)
        );
    }
}
