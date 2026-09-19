//! Cache-field reporting availability. Metadata never changes neutral numeric accounting.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheReadStatus {
    Reported,
    Absent,
    Malformed,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheReadSource {
    OriginUsage,
    CallerSupplied,
    ExplicitCapability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheReadObservation {
    pub status: CacheReadStatus,
    pub source: CacheReadSource,
}

// Schemas describe canonical emitted metadata. Input deserialization remains deliberately
// more forgiving so an unknown future observation never rejects otherwise valid counts.
impl JsonSchema for CacheReadObservation {
    fn schema_name() -> String {
        "CacheReadObservation".into()
    }

    fn json_schema(generator: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "required": ["status", "source"],
            "properties": {
                "status": generator.subschema_for::<CacheReadStatus>(),
                "source": generator.subschema_for::<CacheReadSource>()
            },
            "oneOf": [
                {"properties": {
                    "status": {"enum": ["reported", "absent", "malformed"]},
                    "source": {"const": "origin_usage"}
                }},
                {"properties": {
                    "status": {"enum": ["reported"]},
                    "source": {"const": "caller_supplied"}
                }},
                {"properties": {
                    "status": {"enum": ["unsupported"]},
                    "source": {"const": "explicit_capability"}
                }}
            ]
        }))
        .expect("static cache observation schema")
    }
}

impl CacheReadObservation {
    /// Validate a canonical status/source pair, including manually constructed Rust values.
    pub fn validated(self) -> Option<Self> {
        use CacheReadSource::*;
        use CacheReadStatus::*;
        matches!(
            (self.status, self.source),
            (Reported | Absent | Malformed, OriginUsage)
                | (Reported, CallerSupplied)
                | (Unsupported, ExplicitCapability)
        )
        .then_some(self)
    }

    /// Observation from an inspected origin response; unsupported is never inferred here.
    pub const fn origin(status: CacheReadStatus) -> Self {
        assert!(!matches!(status, CacheReadStatus::Unsupported));
        Self {
            status,
            source: CacheReadSource::OriginUsage,
        }
    }
}

/// Omit invalid hand-constructed metadata at serialization boundaries too.
pub fn cache_read_observation_is_unknown(observation: &Option<CacheReadObservation>) -> bool {
    observation
        .and_then(CacheReadObservation::validated)
        .is_none()
}

/// Optional metadata is deliberately forgiving: new/invalid metadata must not reject old counts.
pub fn deserialize_cache_read_observation<'de, D>(
    deserializer: D,
) -> Result<Option<CacheReadObservation>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(serde_json::from_value::<CacheReadObservation>(value)
        .ok()
        .and_then(CacheReadObservation::validated))
}

/// The caller must establish that a separately reviewed declaration applies to the resolved
/// model/route. Default capability booleans are not declarations. No response remains unknown.
pub fn resolve_cache_read_observation(
    observation: Option<CacheReadObservation>,
    explicitly_unsupported: bool,
) -> Option<CacheReadObservation> {
    observation
        .and_then(CacheReadObservation::validated)
        .map(|o| {
            if explicitly_unsupported && o.status == CacheReadStatus::Absent {
                CacheReadObservation {
                    status: CacheReadStatus::Unsupported,
                    source: CacheReadSource::ExplicitCapability,
                }
            } else {
                o
            }
        })
}

/// Fixed-size coverage over exactly the aggregate's existing call population.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheReadCoverage {
    pub reported: u64,
    pub absent: u64,
    pub malformed: u64,
    pub unsupported: u64,
    pub unknown: u64,
}

impl CacheReadCoverage {
    pub fn unknown(calls: u64) -> Self {
        Self {
            unknown: calls,
            ..Self::default()
        }
    }

    pub fn total(&self) -> u64 {
        self.reported
            .saturating_add(self.absent)
            .saturating_add(self.malformed)
            .saturating_add(self.unsupported)
            .saturating_add(self.unknown)
    }

    /// A malformed/overflowing external aggregate cannot fabricate reporting coverage.
    pub fn normalized(self, calls: u64) -> Self {
        let total = self
            .reported
            .checked_add(self.absent)
            .and_then(|n| n.checked_add(self.malformed))
            .and_then(|n| n.checked_add(self.unsupported))
            .and_then(|n| n.checked_add(self.unknown));
        if total == Some(calls) {
            self
        } else {
            Self::unknown(calls)
        }
    }

    pub fn add(&mut self, observation: Option<CacheReadObservation>) {
        let counter = match observation
            .and_then(CacheReadObservation::validated)
            .map(|o| o.status)
        {
            Some(CacheReadStatus::Reported) => &mut self.reported,
            Some(CacheReadStatus::Absent) => &mut self.absent,
            Some(CacheReadStatus::Malformed) => &mut self.malformed,
            Some(CacheReadStatus::Unsupported) => &mut self.unsupported,
            None => &mut self.unknown,
        };
        *counter = counter.saturating_add(1);
    }

    pub fn merge(&mut self, other: &Self) {
        self.reported = self.reported.saturating_add(other.reported);
        self.absent = self.absent.saturating_add(other.absent);
        self.malformed = self.malformed.saturating_add(other.malformed);
        self.unsupported = self.unsupported.saturating_add(other.unsupported);
        self.unknown = self.unknown.saturating_add(other.unknown);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheReadFamily {
    OpenAi,
    OpenAiResponses,
    Anthropic,
    Gemini,
    Cohere,
    Ollama,
    Bedrock,
}

fn field_status(container: &Value, path: &[&str]) -> CacheReadStatus {
    let mut value = container;
    for key in path {
        if !value.is_object() {
            return CacheReadStatus::Malformed;
        }
        let Some(next) = value.get(*key) else {
            return CacheReadStatus::Absent;
        };
        value = next;
    }
    match value.as_u64() {
        Some(n) if n <= crate::usage::MAX_PLAUSIBLE_TOKENS => CacheReadStatus::Reported,
        _ => CacheReadStatus::Malformed,
    }
}

/// Classify an inspected buffered body independently of whether numeric usage was measurable.
pub fn observe_cache_read(family: CacheReadFamily, response: &Value) -> CacheReadObservation {
    use CacheReadFamily::*;
    let status = match family {
        OpenAi
            if response
                .get("usage")
                .and_then(|u| u.get("prompt_cache_hit_tokens"))
                .is_some() =>
        {
            field_status(response, &["usage", "prompt_cache_hit_tokens"])
        }
        OpenAi => field_status(
            response,
            &["usage", "prompt_tokens_details", "cached_tokens"],
        ),
        OpenAiResponses => field_status(
            response,
            &["usage", "input_tokens_details", "cached_tokens"],
        ),
        Anthropic | Bedrock => field_status(response, &["usage", "cache_read_input_tokens"]),
        Gemini => field_status(response, &["usageMetadata", "cachedContentTokenCount"]),
        Cohere => match response.get("usage") {
            Some(u) if !u.is_object() => CacheReadStatus::Malformed,
            _ => CacheReadStatus::Absent,
        },
        Ollama => CacheReadStatus::Absent,
    };
    CacheReadObservation::origin(status)
}

/// Classify only protocol-significant stream updates. Normal placeholders are not observations.
pub fn observe_cache_read_stream(
    family: CacheReadFamily,
    frame: &Value,
) -> Option<CacheReadObservation> {
    use CacheReadFamily::*;
    let response = match family {
        OpenAi => {
            let usage = frame.get("usage")?;
            if usage.is_null()
                && frame
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|c| !c.is_empty())
            {
                return None;
            }
            frame
        }
        OpenAiResponses => {
            let response = frame.get("response").unwrap_or(frame);
            let usage = response.get("usage")?;
            if usage.is_null()
                && matches!(
                    frame.get("type").and_then(Value::as_str),
                    Some("response.created" | "response.in_progress")
                )
            {
                return None;
            }
            response
        }
        Anthropic => {
            let response = match frame.get("type").and_then(Value::as_str) {
                Some("message_start") => frame.get("message")?,
                Some("message_delta") => frame,
                _ => return None,
            };
            response.get("usage")?;
            response
        }
        Gemini => {
            frame.get("usageMetadata")?;
            frame
        }
        Cohere => {
            let response = if frame.get("usage").is_some() {
                frame
            } else {
                frame.get("delta")?
            };
            response.get("usage")?;
            response
        }
        Ollama => {
            if frame.get("done").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            frame
        }
        Bedrock => {
            frame.get("usage")?;
            frame
        }
    };
    Some(observe_cache_read(family, response))
}

/// Partial updates that omit the cache field cannot erase earlier field-level evidence.
pub fn merge_cache_read_observation(
    current: &mut Option<CacheReadObservation>,
    update: Option<CacheReadObservation>,
) {
    *current = current.and_then(CacheReadObservation::validated);
    if let Some(update) = update.and_then(CacheReadObservation::validated) {
        if update.status != CacheReadStatus::Absent || current.is_none() {
            *current = Some(update);
        }
    }
}

/// The numeric parser's original Option and field metadata are deliberately independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedUsage {
    pub usage: Option<crate::ParsedUsage>,
    pub cache_read_observation: Option<CacheReadObservation>,
}

pub fn observe_usage(family: CacheReadFamily, response: &Value) -> ObservedUsage {
    use CacheReadFamily::*;
    let usage = match family {
        OpenAi => crate::parse_openai_usage(response),
        OpenAiResponses => crate::parse_openai_responses_usage(response),
        Anthropic => crate::parse_anthropic_usage(response),
        Gemini => crate::parse_gemini_usage(response),
        Cohere => crate::parse_cohere_usage(response),
        Ollama => crate::parse_ollama_usage(response),
        Bedrock => crate::parse_bedrock_usage(response),
    };
    ObservedUsage {
        usage,
        cache_read_observation: Some(observe_cache_read(family, response)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, ParsedUsage, RunCostTreeV1, UsageAggregateV1, UsageEvent, UsageV2};
    use serde_json::json;

    fn origin(status: CacheReadStatus) -> Option<CacheReadObservation> {
        Some(CacheReadObservation::origin(status))
    }

    #[test]
    fn emitted_schema_accepts_exactly_the_canonical_status_source_pairs() {
        let schema = serde_json::to_value(schemars::schema_for!(CacheReadObservation)).unwrap();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("status")) && required.contains(&json!("source")));
        let branches = schema["oneOf"].as_array().unwrap();
        assert_eq!(branches.len(), 3);
        for status in [
            CacheReadStatus::Reported,
            CacheReadStatus::Absent,
            CacheReadStatus::Malformed,
            CacheReadStatus::Unsupported,
        ] {
            for source in [
                CacheReadSource::OriginUsage,
                CacheReadSource::CallerSupplied,
                CacheReadSource::ExplicitCapability,
            ] {
                let observation = CacheReadObservation { status, source };
                let value = serde_json::to_value(observation).unwrap();
                let matches = branches
                    .iter()
                    .filter(|branch| {
                        branch["properties"]["status"]["enum"]
                            .as_array()
                            .unwrap()
                            .contains(&value["status"])
                            && branch["properties"]["source"]["const"] == value["source"]
                    })
                    .count();
                assert_eq!(
                    matches,
                    usize::from(observation.validated().is_some()),
                    "{value}"
                );
            }
        }
    }

    #[test]
    fn shared_binding_corpus_matches_rust_observation_boundaries() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../../bindings/fixtures/cache-read-observation-parity.json"
        ))
        .unwrap();
        for case in corpus["manual"].as_array().unwrap() {
            let mut value = json!({"tokens_in":10,"tokens_out":2,"cache_creation_tokens":0,"cache_read_tokens":0,"reasoning_tokens":0});
            if let Some(observation) = case.get("observation") {
                value["cache_read_observation"] = observation.clone();
            }
            let usage: UsageV2 = serde_json::from_value(value.clone()).unwrap();
            let parsed: ParsedUsage = serde_json::from_value(value).unwrap();
            assert_eq!(
                serde_json::to_value(usage.cache_read_observation).unwrap(),
                case["expected"],
                "{}",
                case["name"]
            );
            assert_eq!(parsed.cache_read_observation, usage.cache_read_observation);
            assert_eq!(crate::billable(&usage), 12);
            let event = parsed.apply(UsageEvent::new("r", "t", "p", "m", Backend::External));
            let round_trip: UsageEvent =
                serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
            assert_eq!(
                round_trip.cache_read_observation,
                usage.cache_read_observation
            );
            assert_eq!(event.billable_tokens(), 12);
        }
        for case in corpus["parsed"].as_array().unwrap() {
            let family = match case["provider"].as_str().unwrap() {
                "openai" | "deepseek" => CacheReadFamily::OpenAi,
                "responses" => CacheReadFamily::OpenAiResponses,
                "anthropic" => CacheReadFamily::Anthropic,
                "gemini" => CacheReadFamily::Gemini,
                "ollama" => CacheReadFamily::Ollama,
                other => panic!("add core mapping for shared fixture provider {other}"),
            };
            let observed = observe_usage(family, &case["response"]);
            assert_eq!(
                serde_json::to_value(observed.cache_read_observation.unwrap().status).unwrap(),
                case["status"],
                "{}",
                case["name"]
            );
            assert_eq!(
                observed.usage.unwrap_or_default().cache_read_tokens,
                case["cached"].as_u64().unwrap()
            );
        }
    }

    #[test]
    fn coverage_overflow_buckets_use_call_population_not_latency_samples() {
        let mut aggregator = crate::UsageAggregator::with_cap(crate::Dimension::Provider, 1);
        for (provider, status) in [
            ("a", Some(CacheReadStatus::Reported)),
            ("b", Some(CacheReadStatus::Absent)),
            ("c", None),
        ] {
            let mut event = UsageEvent::new("r", "t", provider, "m", Backend::External);
            event.cache_read_observation = status.map(CacheReadObservation::origin);
            if provider == "a" {
                event.duration_ms = Some(1);
            }
            aggregator.add(&event);
        }
        let total = aggregator.total();
        assert_eq!(total.calls, 3);
        assert_eq!(total.latency.unwrap().samples, 1);
        assert_eq!(
            total.cache_read_coverage,
            Some(CacheReadCoverage {
                reported: 1,
                absent: 1,
                unknown: 1,
                ..Default::default()
            })
        );
        let rows = aggregator.rows();
        let overflow = rows
            .iter()
            .find(|row| row.key == crate::OVERFLOW_KEY)
            .unwrap();
        assert_eq!(overflow.calls, 2);
        assert_eq!(
            overflow.cache_read_coverage,
            Some(CacheReadCoverage {
                absent: 1,
                unknown: 1,
                ..Default::default()
            })
        );
        assert!(rows
            .iter()
            .all(|row| row.cache_read_coverage.unwrap().total() == row.calls));
        let invalid = CacheReadCoverage {
            reported: u64::MAX,
            unknown: 1,
            ..Default::default()
        };
        assert_eq!(
            invalid.normalized(u64::MAX),
            CacheReadCoverage::unknown(u64::MAX)
        );
    }

    #[test]
    fn buffered_field_presence_is_independent_of_numeric_option_and_amount() {
        for family in [
            CacheReadFamily::OpenAi,
            CacheReadFamily::OpenAiResponses,
            CacheReadFamily::Anthropic,
            CacheReadFamily::Gemini,
            CacheReadFamily::Bedrock,
        ] {
            let (container, details, field) = match family {
                CacheReadFamily::OpenAi => {
                    ("usage", Some("prompt_tokens_details"), "cached_tokens")
                }
                CacheReadFamily::OpenAiResponses => {
                    ("usage", Some("input_tokens_details"), "cached_tokens")
                }
                CacheReadFamily::Gemini => ("usageMetadata", None, "cachedContentTokenCount"),
                _ => ("usage", None, "cache_read_input_tokens"),
            };
            assert_eq!(observe_usage(family, &json!({})).usage, None);
            assert_eq!(
                observe_usage(family, &json!({})).cache_read_observation,
                origin(CacheReadStatus::Absent)
            );
            for invalid in [
                Value::Null,
                json!("0"),
                json!(false),
                json!(-1),
                json!(1.5),
                json!(50_000_000_001u64),
            ] {
                let mut usage = json!({field: invalid});
                if let Some(details) = details {
                    usage = json!({details: usage});
                }
                let response = json!({container: usage});
                assert_eq!(
                    observe_cache_read(family, &response).status,
                    CacheReadStatus::Malformed,
                    "{family:?}: {response}"
                );
            }
            for count in [0, 25, 50_000_000_000u64] {
                let mut usage = json!({field: count});
                if let Some(details) = details {
                    usage = json!({details: usage});
                }
                let response = json!({container: usage});
                let observed = observe_usage(family, &response);
                assert_eq!(
                    observed.cache_read_observation,
                    origin(CacheReadStatus::Reported)
                );
                assert_eq!(
                    observed.usage.unwrap().cache_read_observation,
                    observed.cache_read_observation
                );
            }
            for container_value in [Value::Null, json!(false), json!([]), json!("bad")] {
                assert_eq!(
                    observe_cache_read(family, &json!({container: container_value})).status,
                    CacheReadStatus::Malformed
                );
            }
        }
        for family in [CacheReadFamily::Cohere, CacheReadFamily::Ollama] {
            assert_eq!(
                observe_cache_read(family, &json!({})).status,
                CacheReadStatus::Absent
            );
        }
    }

    #[test]
    fn deepseek_precedence_and_invalid_details_do_not_change_numeric_parsing() {
        let response = json!({"usage":{"prompt_tokens":10,"prompt_cache_hit_tokens":null,
            "prompt_tokens_details":{"cached_tokens":8}}});
        let parsed = crate::parse_openai_usage(&response).unwrap();
        assert_eq!((parsed.tokens_in, parsed.cache_read_tokens), (10, 0));
        assert_eq!(
            parsed.cache_read_observation,
            origin(CacheReadStatus::Malformed)
        );
        for value in [Value::Null, json!(false), json!(1), json!([])] {
            let parsed = crate::parse_openai_usage(
                &json!({"usage":{"prompt_tokens":10,"prompt_tokens_details":value}}),
            )
            .unwrap();
            assert_eq!(parsed.tokens_in, 10);
            assert_eq!(
                parsed.cache_read_observation,
                origin(CacheReadStatus::Malformed)
            );
        }
        let parsed = crate::parse_openai_usage(
            &json!({"usage":{"prompt_tokens":1,"prompt_tokens_details":{"cached_tokens":2}}}),
        )
        .unwrap();
        assert_eq!((parsed.tokens_in, parsed.cache_read_tokens), (0, 2));
        assert_eq!(
            parsed.cache_read_observation,
            origin(CacheReadStatus::Reported)
        );
    }

    #[test]
    fn unknown_or_invalid_metadata_never_rejects_numeric_payloads() {
        let base = json!({"tokens_in":4,"tokens_out":2,"cache_creation_tokens":0,"cache_read_tokens":1,"reasoning_tokens":0});
        for metadata in [
            Value::Null,
            json!(false),
            json!([]),
            json!("reported"),
            json!({"status":"future","source":"origin_usage"}),
            json!({"status":"reported","source":"future"}),
            json!({"status":"reported"}),
            json!({"status":"unsupported","source":"origin_usage"}),
            json!({"status":"absent","source":"caller_supplied"}),
        ] {
            let mut value = base.clone();
            value["cache_read_observation"] = metadata;
            let parsed: ParsedUsage = serde_json::from_value(value.clone()).unwrap();
            let neutral: UsageV2 = serde_json::from_value(value).unwrap();
            assert_eq!(parsed.cache_read_observation, None);
            assert_eq!(neutral.cache_read_observation, None);
            assert_eq!(crate::billable(&neutral), 7);
            let mut event =
                serde_json::to_value(UsageEvent::new("r", "t", "p", "m", Backend::External))
                    .unwrap();
            event["cache_read_observation"] = json!({"status":"future","source":"origin_usage"});
            assert_eq!(
                serde_json::from_value::<UsageEvent>(event)
                    .unwrap()
                    .cache_read_observation,
                None
            );
        }
        let invalid = CacheReadObservation {
            status: CacheReadStatus::Unsupported,
            source: CacheReadSource::OriginUsage,
        };
        let usage = UsageV2 {
            cache_read_observation: Some(invalid),
            ..Default::default()
        };
        assert!(serde_json::to_value(usage)
            .unwrap()
            .get("cache_read_observation")
            .is_none());
        for source in [
            CacheReadSource::OriginUsage,
            CacheReadSource::CallerSupplied,
        ] {
            let observation = CacheReadObservation {
                status: CacheReadStatus::Reported,
                source,
            };
            let usage = UsageV2 {
                cache_read_observation: Some(observation),
                ..Default::default()
            };
            assert_eq!(
                serde_json::from_value::<UsageV2>(serde_json::to_value(&usage).unwrap()).unwrap(),
                usage
            );
        }
    }

    #[test]
    fn stream_placeholders_malformed_and_partial_updates_reduce_without_erasure() {
        let mut observed = None;
        let placeholder = json!({"choices":[{"delta":{}}],"usage":null});
        let good = json!({"choices":[],"usage":{"prompt_tokens_details":{"cached_tokens":0}}});
        let bad = json!({"choices":[],"usage":null});
        merge_cache_read_observation(
            &mut observed,
            observe_cache_read_stream(CacheReadFamily::OpenAi, &placeholder),
        );
        assert_eq!(observed, None);
        merge_cache_read_observation(
            &mut observed,
            observe_cache_read_stream(CacheReadFamily::OpenAi, &good),
        );
        merge_cache_read_observation(
            &mut observed,
            observe_cache_read_stream(CacheReadFamily::OpenAi, &placeholder),
        );
        merge_cache_read_observation(
            &mut observed,
            observe_cache_read_stream(
                CacheReadFamily::OpenAi,
                &json!({"usage":{"completion_tokens":5}}),
            ),
        );
        assert_eq!(observed, origin(CacheReadStatus::Reported));
        merge_cache_read_observation(
            &mut observed,
            observe_cache_read_stream(CacheReadFamily::OpenAi, &bad),
        );
        assert_eq!(observed, origin(CacheReadStatus::Malformed));
        merge_cache_read_observation(
            &mut observed,
            observe_cache_read_stream(CacheReadFamily::OpenAi, &good),
        );
        assert_eq!(observed, origin(CacheReadStatus::Reported));
        for kind in ["response.created", "response.in_progress"] {
            assert_eq!(
                observe_cache_read_stream(
                    CacheReadFamily::OpenAiResponses,
                    &json!({"type":kind,"response":{"usage":null}})
                ),
                None
            );
        }
        assert_eq!(
            observe_cache_read_stream(
                CacheReadFamily::OpenAiResponses,
                &json!({"type":"response.completed","response":{"usage":null}})
            ),
            origin(CacheReadStatus::Malformed)
        );
    }

    #[test]
    fn other_stream_families_respect_usage_wrappers_and_input_output_updates() {
        let mut observed = None;
        let start =
            json!({"type":"message_start","message":{"usage":{"cache_read_input_tokens":12}}});
        let delta = json!({"type":"message_delta","usage":{"output_tokens":5}});
        merge_cache_read_observation(
            &mut observed,
            observe_cache_read_stream(CacheReadFamily::Anthropic, &start),
        );
        merge_cache_read_observation(
            &mut observed,
            observe_cache_read_stream(CacheReadFamily::Anthropic, &delta),
        );
        assert_eq!(observed, origin(CacheReadStatus::Reported));
        assert_eq!(
            observe_cache_read_stream(
                CacheReadFamily::Gemini,
                &json!({"usageMetadata":{"cachedContentTokenCount":0}})
            ),
            origin(CacheReadStatus::Reported)
        );
        assert_eq!(
            observe_cache_read_stream(
                CacheReadFamily::Bedrock,
                &json!({"usage":{"cache_read_input_tokens":0}})
            ),
            origin(CacheReadStatus::Reported)
        );
        assert_eq!(
            observe_cache_read_stream(CacheReadFamily::Ollama, &json!({"done":false})),
            None
        );
        assert_eq!(
            observe_cache_read_stream(CacheReadFamily::Ollama, &json!({"done":true})),
            origin(CacheReadStatus::Absent)
        );
        assert_eq!(
            observe_cache_read_stream(
                CacheReadFamily::Cohere,
                &json!({"usage":null,"delta":{"usage":{}}})
            ),
            origin(CacheReadStatus::Malformed)
        );
    }

    #[test]
    fn explicit_capability_never_overrides_wire_or_unknown_response() {
        assert_eq!(resolve_cache_read_observation(None, true), None);
        for status in [CacheReadStatus::Reported, CacheReadStatus::Malformed] {
            assert_eq!(
                resolve_cache_read_observation(origin(status), true),
                origin(status)
            );
        }
        assert_eq!(
            resolve_cache_read_observation(origin(CacheReadStatus::Absent), false),
            origin(CacheReadStatus::Absent)
        );
        assert_eq!(
            resolve_cache_read_observation(origin(CacheReadStatus::Absent), true),
            Some(CacheReadObservation {
                status: CacheReadStatus::Unsupported,
                source: CacheReadSource::ExplicitCapability
            })
        );
    }

    #[test]
    fn conversions_and_coverage_preserve_population_and_legacy_unknown() {
        let mut events = Vec::new();
        for observation in [
            origin(CacheReadStatus::Reported),
            origin(CacheReadStatus::Absent),
            origin(CacheReadStatus::Malformed),
            resolve_cache_read_observation(origin(CacheReadStatus::Absent), true),
            None,
        ] {
            let parsed = ParsedUsage {
                tokens_in: 4,
                tokens_out: 2,
                cache_read_tokens: 1,
                cache_read_observation: observation,
                ..Default::default()
            };
            let usage: UsageV2 = parsed.into();
            let mut event = parsed.apply(UsageEvent::new("r", "t", "p", "m", Backend::External));
            assert_eq!(usage.cache_read_observation, observation);
            assert_eq!(event.cache_read_observation, observation);
            assert_eq!(event.billable_tokens(), 7);
            event.run_id = Some("run".into());
            event.step_id = Some(if events.is_empty() { "parent" } else { "child" }.into());
            if !events.is_empty() {
                event.parent_id = Some("parent".into());
            }
            events.push(event);
        }
        let tree = RunCostTreeV1::from_events("run", &events);
        let expected = CacheReadCoverage {
            reported: 1,
            absent: 1,
            malformed: 1,
            unsupported: 1,
            unknown: 1,
        };
        assert_eq!(tree.total.cache_read_coverage, Some(expected));
        assert_eq!(tree.roots[0].rollup.cache_read_coverage, Some(expected));
        assert_eq!(tree.total.calls, 5);
        assert_eq!(tree.total.billable_tokens, 35);
        let mut old = UsageAggregateV1 {
            calls: 3,
            cache_read_tokens: 99,
            ..Default::default()
        };
        old.merge(&tree.total);
        assert_eq!(old.cache_read_coverage.unwrap().unknown, 4);
        assert_eq!(old.cache_read_coverage.unwrap().total(), old.calls);
        let mut other = tree.total.clone();
        other.merge(&UsageAggregateV1 {
            calls: 2,
            ..Default::default()
        });
        assert_eq!(other.cache_read_coverage.unwrap().unknown, 3);
        let mut invalid = UsageAggregateV1 {
            calls: 3,
            cache_read_coverage: Some(CacheReadCoverage {
                reported: 99,
                ..Default::default()
            }),
            ..Default::default()
        };
        invalid.add(&events[0]);
        assert_eq!(
            invalid.cache_read_coverage.unwrap(),
            CacheReadCoverage {
                reported: 1,
                unknown: 3,
                ..Default::default()
            }
        );
    }
}
