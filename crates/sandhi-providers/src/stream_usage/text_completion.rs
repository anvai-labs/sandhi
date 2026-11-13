//! Text Completion source totals, distinct from Chat wire envelopes.
use super::{StreamObservationError as Error, *};
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct TextCompletion {
    choices: BTreeMap<u64, bool>,
    usage: Option<UsageV2>,
}
impl TextCompletion {
    pub(super) fn event(&mut self, event: &Value) -> Result<Option<UsageV2>, Error> {
        if event.get("object").and_then(Value::as_str) != Some("text_completion")
            || event.get("error").is_some()
        {
            return Err(Error::InvalidEnvelope);
        }
        let choices = event
            .get("choices")
            .and_then(Value::as_array)
            .ok_or(Error::InvalidEnvelope)?;
        if self.usage.is_some() && !choices.is_empty() {
            return Err(Error::DataAfterTerminator);
        }
        for choice in choices {
            let index = choice
                .get("index")
                .and_then(Value::as_u64)
                .filter(|index| *index < 262_144)
                .ok_or(Error::InvalidEnvelope)?;
            if self.choices.len() >= 262_144 && !self.choices.contains_key(&index) {
                return Err(Error::InvalidEnvelope);
            }
            if !choice.get("text").is_some_and(Value::is_string)
                || self.choices.get(&index) == Some(&true)
            {
                return Err(Error::InvalidEnvelope);
            }
            let finished = match choice.get("finish_reason") {
                None | Some(Value::Null) => false,
                Some(Value::String(reason))
                    if matches!(reason.as_str(), "stop" | "length" | "content_filter") =>
                {
                    true
                }
                _ => return Err(Error::InvalidEnvelope),
            };
            self.choices.insert(index, finished);
        }
        if event.get("usage").is_none_or(Value::is_null) {
            return Ok(None);
        }
        if !choices.is_empty()
            || self.choices.is_empty()
            || self.choices.values().any(|finished| !finished)
        {
            return Err(Error::InvalidEnvelope);
        }
        // The counters use the Chat convention, but the operation envelope above
        // is selected by the authorized /completions path, never a response hint.
        let usage = sandhi_core::usage::qualify_buffered_usage(
            EndpointFamilyV1::OpenaiChatCompletions,
            event,
        )
        .map_err(Error::InvalidFamilyUsage)?;
        if self.usage.as_ref().is_some_and(|first| first != &usage) {
            return Err(Error::ConflictingUsage);
        }
        self.usage = Some(usage.clone());
        Ok(Some(usage))
    }
    pub(super) fn finish(&self) -> Result<(), Error> {
        if self.choices.is_empty() || self.choices.values().any(|finished| !finished) {
            return Err(Error::InvalidEnvelope);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_stream_totals_require_native_envelope_finished_choices_and_protocol_terminal() {
        let frame = |value: Value| format!("data: {value}\n\n");
        let content = frame(
            serde_json::json!({"object":"text_completion","choices":[{"index":0,"text":"ok","finish_reason":"stop"}]}),
        );
        let usage = frame(
            serde_json::json!({"object":"text_completion","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11,"completion_tokens_details":{"reasoning_tokens":0}}}),
        );
        let wire = format!("{content}{usage}data: [DONE]\n\n");
        for split in 0..=wire.len() {
            let mut observer = ProtocolStreamUsageObserver::for_text_completion();
            observer.push(&wire.as_bytes()[..split]).unwrap();
            observer.push(&wire.as_bytes()[split..]).unwrap();
            observer.finish().unwrap();
            let counts = observer.canonical_usage().unwrap();
            assert_eq!((counts.tokens_in, counts.tokens_out), (9, 2));
            assert_eq!(counts.reasoning_tokens, Some(0));
            assert_eq!(counts.completeness, UsageCompleteness::Final);
        }
        for invalid in [
            format!("{usage}data: [DONE]\n\n"),
            wire.replace("text_completion", "chat.completion.chunk"),
            wire.replace("\"stop\"", "null"),
            format!("{content}{usage}data: {{"),
            format!("{wire}{content}"),
        ] {
            let mut observer = ProtocolStreamUsageObserver::for_text_completion();
            let result = observer
                .push(invalid.as_bytes())
                .and_then(|_| observer.finish());
            assert!(result.is_err(), "{invalid}");
        }
    }
}
