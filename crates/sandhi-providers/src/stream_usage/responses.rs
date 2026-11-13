//! Reduce only complete Responses events. No prompt/content retention or I/O.
use super::{StreamObservationError as Error, UsageV2, Value};
use sandhi_core::usage::{qualify_buffered_usage, qualify_responses_progress_usage};
use sandhi_core::EndpointFamilyV1;

#[derive(Default)]
pub(super) struct ResponsesUsage {
    id: Option<String>,
    pub done: bool,
    pub provider_failed: bool,
    tail_done: bool,
    previous: Option<UsageV2>,
    progress: Value,
    sequence: Option<u64>,
}

impl ResponsesUsage {
    pub fn tail_done(&mut self, done: bool) -> Result<(), Error> {
        if !done {
            return Err(Error::MissingTerminator);
        }
        if self.tail_done {
            return Err(Error::DataAfterTerminator);
        }
        self.tail_done = true;
        Ok(())
    }

    pub fn event(
        &mut self,
        event: &Value,
        message: bool,
        name: Option<&str>,
    ) -> Result<Option<UsageV2>, Error> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or(Error::InvalidEnvelope)?;
        if !message && name != Some(kind) {
            return Err(Error::UnexpectedEventType);
        }
        if let Some(sequence) = event.get("sequence_number") {
            let sequence = sequence.as_u64().ok_or(Error::InvalidEnvelope)?;
            if self.sequence.is_some_and(|previous| sequence <= previous) {
                return Err(Error::InvalidEnvelope);
            }
            self.sequence = Some(sequence);
        }
        if kind == "error" {
            self.provider_failed = true;
            return Err(Error::ProviderEvent);
        }
        if !kind.starts_with("response.") {
            return Err(Error::UnexpectedEventType);
        }
        if let Some(id) = event.get("response_id") {
            let id = valid_id(id)?;
            self.identity(Some(id), false)?;
        }
        let status = match kind {
            "response.completed" => Some("completed"),
            "response.incomplete" => Some("incomplete"),
            "response.failed" => Some("failed"),
            "response.created" | "response.queued" | "response.in_progress" => None,
            _ => return Ok(None), // Preserve content/tool/future events, never meter them as totals.
        };
        let response = event
            .get("response")
            .filter(|r| r.is_object())
            .ok_or(Error::InvalidEnvelope)?;
        let id = response.get("id").map(valid_id).transpose()?;
        self.identity(id, status.is_some())?;
        let update = if let Some(status) = status {
            if response
                .get("status")
                .is_some_and(|s| s.as_str() != Some(status))
            {
                return Err(Error::InvalidEnvelope);
            }
            self.done = true;
            self.provider_failed = status == "failed";
            qualify_buffered_usage(EndpointFamilyV1::OpenaiResponses, response)
                .map(Some)
                .map_err(Error::InvalidFamilyUsage)?
        } else {
            if response
                .get("status")
                .is_some_and(|s| !matches!(s.as_str(), Some("queued" | "in_progress")))
            {
                return Err(Error::InvalidEnvelope);
            }
            // Validate supplied values before merging. Omitted progress fields
            // mean unknown, not a reset of a previously reported counter.
            if qualify_responses_progress_usage(response)
                .map_err(Error::InvalidFamilyUsage)?
                .is_none()
            {
                return Ok(None);
            }
            merge_progress(&mut self.progress, response);
            qualify_responses_progress_usage(&self.progress).map_err(Error::InvalidFamilyUsage)?
        };
        if let Some(update) = &update {
            if self.previous.as_ref().is_some_and(|previous| {
                inclusive_input(update) < inclusive_input(previous)
                    || update.tokens_out < previous.tokens_out
                    || update.cache_read_tokens < previous.cache_read_tokens
                    || update.cache_creation_tokens < previous.cache_creation_tokens
                    || update.reasoning_tokens.unwrap_or(0) < previous.reasoning_tokens.unwrap_or(0)
            }) {
                return Err(Error::InvalidFamilyUsage(
                    sandhi_core::usage::BufferedUsageError::InconsistentUsage,
                ));
            }
            self.previous = Some(update.clone());
        }
        Ok(update)
    }

    fn identity(&mut self, id: Option<&str>, terminal: bool) -> Result<(), Error> {
        if self
            .id
            .as_ref()
            .is_some_and(|known| id.is_some_and(|id| known != id) || (terminal && id.is_none()))
        {
            return Err(Error::IdentityMismatch);
        }
        if self.id.is_none() {
            self.id = id.map(str::to_owned);
        }
        Ok(())
    }
}

fn valid_id(id: &Value) -> Result<&str, Error> {
    id.as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .ok_or(Error::InvalidEnvelope)
}

fn inclusive_input(usage: &UsageV2) -> u64 {
    usage.tokens_in + usage.cache_read_tokens + usage.cache_creation_tokens
}

fn merge_progress(known: &mut Value, response: &Value) {
    if known.is_null() {
        *known = serde_json::json!({"usage": {
            "input_tokens_details": {}, "output_tokens_details": {}
        }});
    }
    // Retain five bounded category counters only, never response content/state.
    // total_tokens describes its own event; validate it before merging, but do
    // not carry a stale total forward when another category advances.
    for path in [
        "/usage/input_tokens",
        "/usage/output_tokens",
        "/usage/input_tokens_details/cached_tokens",
        "/usage/input_tokens_details/cache_write_tokens",
        "/usage/output_tokens_details/reasoning_tokens",
    ] {
        if let Some(value) = response.pointer(path) {
            let (parent, field) = path.rsplit_once('/').expect("static counter path");
            known.pointer_mut(parent).unwrap()[field] = value.clone();
        }
    }
    // Older inclusive counters are lower bounds when a later event only
    // reports a growing subset. Do not treat an omitted parent as a new zero
    // or freeze it at an earlier value and reject valid partial progress.
    for (parent, subsets) in [
        (
            "/usage/input_tokens",
            &[
                "/usage/input_tokens_details/cached_tokens",
                "/usage/input_tokens_details/cache_write_tokens",
            ][..],
        ),
        (
            "/usage/output_tokens",
            &["/usage/output_tokens_details/reasoning_tokens"][..],
        ),
    ] {
        if response.pointer(parent).is_none() {
            let lower = subsets
                .iter()
                .filter_map(|path| known.pointer(path).and_then(Value::as_u64))
                .sum::<u64>();
            let previous = known.pointer(parent).and_then(Value::as_u64).unwrap_or(0);
            let (root, field) = parent.rsplit_once('/').unwrap();
            known.pointer_mut(root).unwrap()[field] = Value::from(previous.max(lower));
        }
    }
}
