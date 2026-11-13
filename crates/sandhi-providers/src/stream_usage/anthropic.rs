//! Messages cumulative counters only. Never retain content, tools or credentials.
use super::{StreamObservationError as Error, UsageV2, Value};
use sandhi_core::usage::{
    qualify_anthropic_progress_usage, qualify_buffered_usage_with_profile, AnthropicUsageProfile,
    BufferedUsageError,
};
use sandhi_core::EndpointFamilyV1;

const COUNTERS: &[&str] = &[
    "/usage/input_tokens",
    "/usage/output_tokens",
    "/usage/cache_read_input_tokens",
    "/usage/cache_creation_input_tokens",
    "/usage/output_tokens_details/thinking_tokens",
    "/usage/cache_creation/ephemeral_5m_input_tokens",
    "/usage/cache_creation/ephemeral_1h_input_tokens",
];

#[derive(Default)]
pub(super) struct AnthropicUsage {
    id: Option<String>,
    known: Value,
    terminal_delta: bool,
    pub done: bool,
    pub provider_failed: bool,
}

impl AnthropicUsage {
    pub fn event(
        &mut self,
        event: &Value,
        message: bool,
        name: Option<&str>,
        profile: AnthropicUsageProfile,
    ) -> Result<Option<UsageV2>, Error> {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or(Error::InvalidEnvelope)?;
        if !message && name != Some(kind) {
            return Err(Error::UnexpectedEventType);
        }
        if kind == "error" {
            self.provider_failed = true;
            return Err(Error::ProviderEvent);
        }
        if let Some(id) = event.get("message_id") {
            if self.id.as_deref() != Some(valid_id(id)?) {
                return Err(Error::IdentityMismatch);
            }
        }
        match kind {
            "ping" => Ok(None),
            "message_start" => {
                if self.id.is_some() {
                    return Err(Error::IdentityMismatch);
                }
                let start = event.get("message").ok_or(Error::InvalidEnvelope)?;
                if start.get("type").and_then(Value::as_str) != Some("message")
                    || start.get("role").and_then(Value::as_str) != Some("assistant")
                    || start.get("stop_reason").is_some_and(|v| !v.is_null())
                {
                    return Err(Error::InvalidEnvelope);
                }
                self.id = Some(valid_id(start.get("id").ok_or(Error::InvalidEnvelope)?)?.into());
                self.update(start, profile)
            }
            "message_delta" => {
                self.require_started()?;
                if self.terminal_delta {
                    return Err(Error::InvalidEnvelope);
                }
                let delta = event
                    .get("delta")
                    .filter(|v| v.is_object())
                    .ok_or(Error::InvalidEnvelope)?;
                if let Some(reason) = delta.get("stop_reason").filter(|v| !v.is_null()) {
                    valid_id(reason)?;
                    // A stop reason cannot promote an initial/progress output
                    // count: the terminating delta must explicitly report it.
                    if event.pointer("/usage/output_tokens").is_none() {
                        return Err(Error::InvalidEnvelope);
                    }
                    self.terminal_delta = true;
                }
                self.update(event, profile)
            }
            "message_stop" => {
                self.require_started()?;
                if !self.terminal_delta {
                    return Err(Error::InvalidEnvelope);
                }
                self.done = true;
                let mut final_counts = self.known.clone();
                // TTL counters may come from different cumulative updates. Their
                // sum is a lower bound, not a same-frame equality with a newer
                // aggregate. Each actual frame was checked before merging.
                let split = self.split_write();
                if self
                    .known
                    .pointer("/usage/cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .is_some_and(|written| split > written)
                {
                    return Err(Error::InvalidFamilyUsage(
                        BufferedUsageError::InconsistentUsage,
                    ));
                }
                if let Some(usage) = final_counts.get_mut("usage").and_then(Value::as_object_mut) {
                    usage.remove("cache_creation");
                }
                qualify_buffered_usage_with_profile(
                    EndpointFamilyV1::AnthropicMessages,
                    profile,
                    &final_counts,
                )
                .map(Some)
                .map_err(Error::InvalidFamilyUsage)
            }
            "content_block_start" | "content_block_delta" | "content_block_stop" => {
                self.require_started()?;
                if self.terminal_delta
                    || event.pointer("/content_block/type").and_then(Value::as_str)
                        == Some("fallback")
                {
                    return Err(Error::InvalidEnvelope);
                }
                Ok(None)
            }
            // Future named events are forwarded by the byte owner but cannot
            // supply totals, begin another message or authorize settlement.
            _ => Ok(None),
        }
    }

    fn require_started(&self) -> Result<(), Error> {
        self.id.as_ref().map(|_| ()).ok_or(Error::InvalidEnvelope)
    }

    fn split_write(&self) -> u64 {
        COUNTERS[5..]
            .iter()
            .filter_map(|path| self.known.pointer(path).and_then(Value::as_u64))
            .sum()
    }

    fn update(
        &mut self,
        event: &Value,
        profile: AnthropicUsageProfile,
    ) -> Result<Option<UsageV2>, Error> {
        if qualify_anthropic_progress_usage(event, profile)
            .map_err(Error::InvalidFamilyUsage)?
            .is_none()
        {
            return Ok(None);
        }
        let mut next = if self.known.is_null() {
            serde_json::json!({"usage":{"cache_creation":{},"output_tokens_details":{}}})
        } else {
            self.known.clone()
        };
        for path in COUNTERS {
            if let Some(value) = event.pointer(path) {
                if self
                    .known
                    .pointer(path)
                    .and_then(Value::as_u64)
                    .is_some_and(|previous| value.as_u64().is_some_and(|n| n < previous))
                {
                    return Err(Error::InvalidFamilyUsage(
                        BufferedUsageError::InconsistentUsage,
                    ));
                }
                let (parent, field) = path.rsplit_once('/').expect("static counter path");
                next.pointer_mut(parent).expect("bounded counter object")[field] = value.clone();
            }
        }
        // For progress only, older aggregate counters are lower bounds when a
        // newer subset grows. Keep their actual reported presence/value in
        // `known`: the terminal qualifier must never see invented parent counts.
        let mut view = next.clone();
        let split: u64 = COUNTERS[5..]
            .iter()
            .filter_map(|path| next.pointer(path).and_then(Value::as_u64))
            .sum();
        let written = next.pointer(COUNTERS[3]).and_then(Value::as_u64);
        if let Some(usage) = view.get_mut("usage").and_then(Value::as_object_mut) {
            usage.remove("cache_creation");
            if COUNTERS[5..]
                .iter()
                .any(|path| next.pointer(path).is_some())
                || written.is_some()
            {
                usage.insert(
                    "cache_creation_input_tokens".into(),
                    written.unwrap_or(0).max(split).into(),
                );
            }
        }
        if let Some(reasoning) = next.pointer(COUNTERS[4]).and_then(Value::as_u64) {
            let output = next
                .pointer(COUNTERS[1])
                .and_then(Value::as_u64)
                .unwrap_or(0);
            view["usage"]["output_tokens"] = output.max(reasoning).into();
        }
        let update =
            qualify_anthropic_progress_usage(&view, profile).map_err(Error::InvalidFamilyUsage)?;
        self.known = next;
        Ok(update)
    }
}

fn valid_id(value: &Value) -> Result<&str, Error> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .ok_or(Error::InvalidEnvelope)
}
