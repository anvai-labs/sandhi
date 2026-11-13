//! Strict terminal observations for Cohere SSE and Ollama NDJSON. The portable
//! projector separately owns content fidelity; counters qualify only at clean EOF.
use super::StreamObservationError as Error;
use sandhi_core::{EndpointFamilyV1 as Family, UsageCompleteness, UsageV2};
use serde_json::{json, Value};
#[derive(Default)]
pub(super) struct NativeChatUsage {
    started: bool,
    identity: Option<String>,
    pub(super) done: bool,
    pub(super) provider_failed: bool,
    pub(super) usage: Option<UsageV2>,
}
impl NativeChatUsage {
    pub(super) fn event(
        &mut self,
        family: Family,
        event: &Value,
    ) -> Result<Option<UsageV2>, Error> {
        if self.done {
            return Err(Error::DataAfterTerminator);
        }
        if !event.is_object() {
            return Err(Error::InvalidEnvelope);
        }
        if event.get("error").is_some() {
            self.provider_failed = true;
            return Err(Error::ProviderEvent);
        }
        let terminal = match family {
            Family::CohereChat => match event.get("type").and_then(Value::as_str) {
                Some("message-start") if !self.started => {
                    self.started = true;
                    false
                }
                Some("message-end") if self.started => true,
                Some(
                    "content-start" | "content-delta" | "content-end" | "tool-plan-delta"
                    | "tool-call-start" | "tool-call-delta" | "tool-call-end" | "citation-start"
                    | "citation-end",
                ) if self.started => false,
                _ => return Err(Error::InvalidEnvelope),
            },
            Family::OllamaChat => {
                let model = event
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .ok_or(Error::InvalidEnvelope)?;
                match &self.identity {
                    Some(first) if first != model => return Err(Error::IdentityMismatch),
                    None => self.identity = Some(model.into()),
                    _ => {}
                }
                event
                    .get("done")
                    .and_then(Value::as_bool)
                    .ok_or(Error::InvalidEnvelope)?
            }
            _ => return Err(Error::UnsupportedEndpoint),
        };
        if !terminal {
            if event.get("usage").is_some()
                || event.pointer("/delta/usage").is_some()
                || event.get("prompt_eval_count").is_some()
                || event.get("eval_count").is_some()
            {
                return Err(Error::InvalidEnvelope);
            }
            return Ok(None);
        }
        let response = if family == Family::CohereChat {
            json!({"usage":event.pointer("/delta/usage").ok_or(Error::InvalidEnvelope)?})
        } else {
            event.clone()
        };
        let mut usage = sandhi_core::usage::qualify_buffered_usage(family, &response)
            .map_err(Error::InvalidFamilyUsage)?;
        usage.completeness = UsageCompleteness::Partial;
        self.done = true;
        self.usage = Some(usage.clone());
        Ok(Some(usage))
    }
}
