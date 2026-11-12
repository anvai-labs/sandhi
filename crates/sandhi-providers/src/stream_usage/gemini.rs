//! GenerateContent counters and candidate lifecycle only; never retain content.
use super::{StreamObservationError as Error, UsageV2, Value};
use sandhi_core::usage::{
    qualify_buffered_usage, qualify_gemini_progress_usage, BufferedUsageError,
};
use sandhi_core::EndpointFamilyV1;
use std::collections::{BTreeMap, BTreeSet};

const COUNTERS: &[&str] = &[
    "promptTokenCount",
    "candidatesTokenCount",
    "cachedContentTokenCount",
    "thoughtsTokenCount",
    "toolUsePromptTokenCount",
    "totalTokenCount",
];
const MAX_CANDIDATES: u64 = 64;

#[derive(Default)]
pub(super) struct GeminiUsage {
    id: Option<String>,
    model: Option<String>,
    candidates: BTreeMap<u64, bool>,
    blocked: bool,
    known: serde_json::Map<String, Value>,
    final_frame: Option<Value>,
    pub provider_failed: bool,
}

impl GeminiUsage {
    pub fn event(&mut self, event: &Value, message: bool) -> Result<Option<UsageV2>, Error> {
        if !message {
            return Err(Error::UnexpectedEventType);
        }
        if !event.is_object() {
            return Err(Error::InvalidEnvelope);
        }
        if event.get("error").is_some() {
            self.provider_failed = true;
            return Err(Error::ProviderEvent);
        }
        if ![
            "candidates",
            "usageMetadata",
            "responseId",
            "modelVersion",
            "promptFeedback",
            "createTime",
        ]
        .iter()
        .any(|key| event.get(key).is_some())
        {
            return Err(Error::InvalidEnvelope);
        }
        identity(&mut self.id, event.get("responseId"))?;
        identity(&mut self.model, event.get("modelVersion"))?;
        let candidates = event
            .get("candidates")
            .map(|v| v.as_array().ok_or(Error::InvalidEnvelope))
            .transpose()?;
        let mut seen = BTreeSet::new();
        if let Some(candidates) = candidates {
            if candidates.len() > MAX_CANDIDATES as usize {
                return Err(Error::InvalidEnvelope);
            }
            if !candidates.is_empty() {
                self.final_frame = None;
            }
            for candidate in candidates {
                if self.blocked || !candidate.is_object() {
                    return Err(Error::InvalidEnvelope);
                }
                let index = match candidate.get("index") {
                    None if candidates.len() == 1 => 0,
                    Some(v) => v.as_u64().ok_or(Error::InvalidEnvelope)?,
                    _ => return Err(Error::InvalidEnvelope),
                };
                if index >= MAX_CANDIDATES || !seen.insert(index) {
                    return Err(Error::InvalidEnvelope);
                }
                if self.candidates.get(&index) == Some(&true) {
                    return Err(Error::DataAfterTerminator);
                }
                let finished = match candidate.get("finishReason") {
                    None => false,
                    Some(Value::String(reason))
                        if reason.is_empty() || reason == "FINISH_REASON_UNSPECIFIED" =>
                    {
                        false
                    }
                    Some(Value::String(reason))
                        if matches!(
                            reason.as_str(),
                            "STOP"
                                | "MAX_TOKENS"
                                | "SAFETY"
                                | "RECITATION"
                                | "OTHER"
                                | "BLOCKLIST"
                                | "PROHIBITED_CONTENT"
                                | "SPII"
                                | "MALFORMED_FUNCTION_CALL"
                                | "MODEL_ARMOR"
                                | "IMAGE_SAFETY"
                                | "IMAGE_PROHIBITED_CONTENT"
                                | "IMAGE_RECITATION"
                                | "IMAGE_OTHER"
                                | "UNEXPECTED_TOOL_CALL"
                                | "NO_IMAGE"
                                | "MALFORMED_RESPONSE"
                                | "CONTINUATION"
                                | "TOO_MANY_TOOL_CALLS"
                        ) =>
                    {
                        true
                    }
                    _ => return Err(Error::InvalidEnvelope),
                };
                self.candidates.insert(index, finished);
            }
        }
        if let Some(feedback) = event.get("promptFeedback") {
            if !feedback.is_object() {
                return Err(Error::InvalidEnvelope);
            }
            if let Some(reason) = feedback.get("blockReason") {
                let reason = valid_string(reason)?;
                if reason != "BLOCK_REASON_UNSPECIFIED" {
                    if !matches!(
                        reason,
                        "SAFETY"
                            | "OTHER"
                            | "BLOCKLIST"
                            | "PROHIBITED_CONTENT"
                            | "IMAGE_SAFETY"
                            | "MODEL_ARMOR"
                    ) {
                        return Err(Error::InvalidEnvelope);
                    }
                    if self.blocked || !self.candidates.is_empty() {
                        return Err(Error::InvalidEnvelope);
                    }
                    self.blocked = true;
                }
            }
        }
        self.update(event)
    }

    fn terminal(&self) -> bool {
        self.blocked || (!self.candidates.is_empty() && self.candidates.values().all(|v| *v))
    }

    fn update(&mut self, event: &Value) -> Result<Option<UsageV2>, Error> {
        qualify_gemini_progress_usage(event).map_err(Error::InvalidFamilyUsage)?;
        if !COUNTERS
            .iter()
            .any(|key| event["usageMetadata"].get(key).is_some())
        {
            return Ok(None);
        }
        let mut next = self.known.clone();
        for key in COUNTERS {
            if let Some(value) = event["usageMetadata"].get(key) {
                if self
                    .known
                    .get(*key)
                    .and_then(Value::as_u64)
                    .is_some_and(|n| value.as_u64().is_some_and(|v| v < n))
                {
                    return Err(Error::InvalidFamilyUsage(
                        BufferedUsageError::InconsistentUsage,
                    ));
                }
                next.insert((*key).into(), value.clone());
            }
        }
        // Across progress frames a newer subset can exceed an older aggregate.
        // The adjusted view is only a lower bound, never final reported evidence.
        let mut view = next.clone();
        if let Some(cached) = next.get("cachedContentTokenCount").and_then(Value::as_u64) {
            if let Some(prompt) = view.get_mut("promptTokenCount") {
                *prompt = cached.max(prompt.as_u64().unwrap_or(0)).into();
            }
        }
        // A total from a different frame is not an equality with newer components.
        view.remove("totalTokenCount");
        let update = qualify_gemini_progress_usage(&serde_json::json!({"usageMetadata":view}))
            .map_err(Error::InvalidFamilyUsage)?;
        self.known = next;
        // Only a self-contained usage frame at/after candidate completion can
        // qualify at EOF. Earlier counters cannot fill missing final parents.
        self.final_frame = self.terminal().then(|| {
            let counts: serde_json::Map<String, Value> = COUNTERS
                .iter()
                .filter_map(|key| {
                    event["usageMetadata"]
                        .get(key)
                        .map(|v| ((*key).into(), v.clone()))
                })
                .collect();
            serde_json::json!({"usageMetadata":counts})
        });
        Ok(update)
    }

    pub fn finish(&self) -> Result<UsageV2, Error> {
        if !self.terminal() {
            return Err(Error::MissingTerminator);
        }
        let frame = self.final_frame.as_ref().ok_or(Error::InvalidEnvelope)?;
        let final_counts = &frame["usageMetadata"];
        // totalTokenCount is additional terminal evidence for this conservative
        // stream profile. Buffered qualification remains backward compatible.
        if final_counts.get("totalTokenCount").is_none() {
            return Err(Error::InvalidEnvelope);
        }
        for key in COUNTERS {
            if let Some(previous) = self.known.get(*key) {
                // A known optional zero may be omitted, but a nonzero count may
                // never disappear from the self-contained final measurement.
                if final_counts.get(key).is_none() && previous.as_u64() == Some(0) {
                    continue;
                }
                if final_counts.get(key) != Some(previous) {
                    return Err(Error::InvalidFamilyUsage(
                        BufferedUsageError::InconsistentUsage,
                    ));
                }
            }
        }
        let mut frame = frame.clone();
        for key in &COUNTERS[2..5] {
            if frame["usageMetadata"].get(key).is_none()
                && self.known.get(*key) == Some(&Value::from(0))
            {
                frame["usageMetadata"][*key] = 0.into();
            }
        }
        let usage = qualify_buffered_usage(EndpointFamilyV1::GeminiGenerateContent, &frame)
            .map_err(Error::InvalidFamilyUsage)?;
        if self.blocked && (usage.tokens_out != 0 || usage.reasoning_tokens.unwrap_or(0) != 0) {
            return Err(Error::InvalidEnvelope);
        }
        Ok(usage)
    }
}

fn valid_string(value: &Value) -> Result<&str, Error> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .ok_or(Error::InvalidEnvelope)
}

fn identity(known: &mut Option<String>, value: Option<&Value>) -> Result<(), Error> {
    if let Some(value) = value {
        let value = valid_string(value)?;
        if known.as_deref().is_some_and(|previous| previous != value) {
            return Err(Error::IdentityMismatch);
        }
        if known.is_none() {
            *known = Some(value.into());
        }
    }
    Ok(())
}
