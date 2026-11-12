//! Buffered delivery can use complete item snapshots, never partial replay state.
use crate::ProviderError;
use serde_json::Value;
use std::collections::BTreeMap;

fn invalid() -> ProviderError {
    ProviderError::OutputProjection(
        "Responses stream cannot be assembled without losing native state; use native streaming"
            .into(),
    )
}

#[derive(Default)]
pub(super) struct NativeAggregate {
    items: BTreeMap<u32, (Value, bool, usize)>,
    response: Option<Value>,
    retained: usize,
    response_id: Option<String>,
    model: Option<String>,
    materialized: BTreeMap<(u32, u32, u8), (String, bool)>,
    validate_materialized: bool,
    item_ids: BTreeMap<u32, String>,
}

// Count serialized bytes without allocating another copy of a bounded frame.
struct Count(usize);
impl std::io::Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn size(value: &Value) -> Result<usize, ProviderError> {
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).map_err(|_| invalid())?;
    Ok(count.0)
}
fn meaningful(value: &Value) -> bool {
    !(value.is_null()
        || value.as_str().is_some_and(str::is_empty)
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty))
}

// Added encrypted_content is expressly incomplete. A complete item may replace
// it; omission is still loss. Other opaque state must survive unchanged.
fn covers(prior: &Value, complete: &Value, key: &str) -> bool {
    if !meaningful(prior) {
        return true;
    }
    if key == "status" {
        return complete.is_string();
    }
    if key == "encrypted_content" {
        return complete.as_str().is_some_and(|s| !s.is_empty());
    }
    match (prior, complete) {
        (Value::Object(a), Value::Object(b)) => a
            .iter()
            .all(|(key, value)| covers(value, b.get(key).unwrap_or(&Value::Null), key)),
        (Value::Array(a), Value::Array(b)) => {
            a.len() <= b.len() && a.iter().zip(b).all(|(a, b)| covers(a, b, ""))
        }
        (Value::String(a), Value::String(b)) if matches!(key, "text" | "refusal" | "arguments") => {
            b.starts_with(a)
        }
        _ => prior == complete,
    }
}
impl NativeAggregate {
    fn item_identity(&mut self, index: u32, value: &Value, key: &str) -> Result<(), ProviderError> {
        if !self.validate_materialized || value.get(key).is_none_or(Value::is_null) {
            return Ok(());
        }
        let id = crate::buffered_fidelity::id(value, key).map_err(|_| invalid())?;
        if let Some(prior) = self.item_ids.get(&index) {
            return if prior == id { Ok(()) } else { Err(invalid()) };
        }
        if self.item_ids.len() >= 1024
            || self.item_ids.values().any(|prior| prior == id)
            || self.retained.saturating_add(id.len()) > crate::MAX_STREAM_LINE_BYTES
        {
            return Err(invalid());
        }
        self.retained += id.len();
        self.item_ids.insert(index, id.to_owned());
        Ok(())
    }
    pub(super) fn for_delivery(portable: bool) -> Self {
        Self {
            validate_materialized: !portable,
            ..Self::default()
        }
    }
    fn materialize(&mut self, event: &Value) -> Result<(), ProviderError> {
        if !self.validate_materialized {
            return Ok(());
        }
        let kind = event["type"].as_str().ok_or_else(invalid)?;
        let complete = kind.ends_with(".done");
        let number = |key| -> Result<u32, ProviderError> {
            match event.get(key) {
                None => Ok(0),
                Some(v) => v
                    .as_u64()
                    .and_then(|v| u32::try_from(v).ok())
                    .ok_or_else(invalid),
            }
        };
        let (channel, part, value) =
            if kind.contains("content_part") || kind.contains("reasoning_summary_part") {
                let part = event.get("part").ok_or_else(invalid)?;
                let channel = match part["type"].as_str() {
                    Some("output_text") => 0,
                    Some("refusal") => 1,
                    Some("summary_text") => 2,
                    Some("reasoning_text") => 3,
                    _ => return Err(invalid()),
                };
                crate::buffered_fidelity::fields(
                    part,
                    &["type", if channel == 1 { "refusal" } else { "text" }],
                )
                .map_err(|_| invalid())?;
                (
                    channel,
                    number(if channel == 2 {
                        "summary_index"
                    } else {
                        "content_index"
                    })?,
                    part[if channel == 1 { "refusal" } else { "text" }]
                        .as_str()
                        .ok_or_else(invalid)?,
                )
            } else {
                let channel = if kind.contains("output_text") {
                    0
                } else if kind.contains("refusal") {
                    1
                } else if kind.contains("reasoning_summary") {
                    2
                } else if kind.contains("reasoning_text") {
                    3
                } else {
                    4
                };
                let value = event[if !complete {
                    "delta"
                } else if channel == 1 {
                    "refusal"
                } else if channel == 4 {
                    "arguments"
                } else {
                    "text"
                }]
                .as_str()
                .ok_or_else(invalid)?;
                (
                    channel,
                    number(if channel == 2 {
                        "summary_index"
                    } else {
                        "content_index"
                    })?,
                    value,
                )
            };
        let index = number("output_index")?;
        self.item_identity(index, event, "item_id")?;
        let key = (index, part, channel);
        let snapshot = complete || kind.ends_with(".added");
        let prior = self.materialized.get(&key);
        if prior.is_none() && self.materialized.len() >= 1024 {
            return Err(invalid());
        }
        let prior_len = prior.map_or(0, |v| v.0.len());
        if prior.is_some_and(|(prior, done)| {
            (*done && ((!snapshot && !value.is_empty()) || (snapshot && value != prior)))
                || (snapshot && !value.starts_with(prior))
        }) {
            return Err(invalid());
        }
        let extra = if snapshot {
            value.len().saturating_sub(prior_len)
        } else {
            value.len()
        };
        if self.retained.saturating_add(extra) > crate::MAX_STREAM_LINE_BYTES {
            return Err(invalid());
        }
        self.retained += extra;
        let stored = self.materialized.entry(key).or_default();
        if snapshot {
            stored.0.push_str(&value[prior_len..]);
        } else {
            stored.0.push_str(value);
        }
        stored.1 |= complete;
        Ok(())
    }
    fn identity(&mut self, response: &Value) -> Result<(), ProviderError> {
        for (key, slot) in [("id", &mut self.response_id), ("model", &mut self.model)] {
            if let Some(value) = response.get(key).filter(|v| !v.is_null()) {
                let value = value
                    .as_str()
                    .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
                    .ok_or_else(invalid)?;
                if slot.as_deref().is_some_and(|prior| prior != value) {
                    return Err(invalid());
                }
                if slot.is_none() {
                    let retained = self.retained.saturating_add(value.len());
                    if retained > crate::MAX_STREAM_LINE_BYTES {
                        return Err(invalid());
                    }
                    self.retained = retained;
                    *slot = Some(value.to_owned());
                }
            }
        }
        Ok(())
    }
    fn item(&mut self, index: u32, item: &Value, done: bool) -> Result<(), ProviderError> {
        if !item.is_object() {
            return Err(invalid());
        }
        self.item_identity(index, item, "id")?;
        if let Some((prior, was_done, _)) = self.items.get(&index) {
            if *was_done || !covers(prior, item, "") {
                return Err(invalid());
            }
        } else if self.items.len() >= 1024 {
            return Err(invalid());
        }
        let bytes = size(item)?;
        let retained = self
            .retained
            .saturating_sub(self.items.get(&index).map_or(0, |s| s.2))
            .saturating_add(bytes);
        if retained > crate::MAX_STREAM_LINE_BYTES {
            return Err(invalid());
        }
        self.retained = retained;
        let complete = done || self.items.get(&index).is_some_and(|s| s.1);
        self.items.insert(index, (item.clone(), complete, bytes));
        Ok(())
    }
    pub(super) fn observe(&mut self, event: &Value) -> Result<(), ProviderError> {
        if self.response.is_some() {
            return Err(invalid());
        }
        if self.validate_materialized {
            crate::buffered_fidelity::fields(
                event,
                &[
                    "type",
                    "sequence_number",
                    "response_id",
                    "response",
                    "output_index",
                    "item_id",
                    "content_index",
                    "summary_index",
                    "item",
                    "part",
                    "delta",
                    "text",
                    "refusal",
                    "arguments",
                    "obfuscation",
                ],
            )
            .map_err(|_| invalid())?;
            if event
                .get("obfuscation")
                .is_some_and(|v| !v.is_null() && !v.is_string())
            {
                return Err(invalid());
            }
        }
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.added" | "response.output_item.done") => {
                let index = event
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(invalid)?;
                let item = event
                    .get("item")
                    .filter(|v| v.is_object())
                    .ok_or_else(invalid)?;
                let done = event["type"] == "response.output_item.done";
                self.item(index, item, done)?;
            }
            Some("response.created" | "response.in_progress" | "response.queued") => {
                let response = event
                    .get("response")
                    .filter(|v| v.is_object())
                    .ok_or_else(invalid)?;
                if self.validate_materialized {
                    crate::buffered_fidelity::fields(
                        response,
                        crate::buffered_fidelity::RESPONSES_FIELDS,
                    )
                    .map_err(|_| invalid())?;
                }
                self.identity(response)?;
                if let Some(items) = response.get("output").filter(|v| !v.is_null()) {
                    let items = items
                        .as_array()
                        .filter(|v| v.len() <= 1024)
                        .ok_or_else(invalid)?;
                    for (i, item) in items.iter().enumerate() {
                        self.item(i as u32, item, false)?;
                    }
                }
            }
            Some("response.completed" | "response.incomplete" | "response.failed") => {
                let response = event
                    .get("response")
                    .filter(|v| v.is_object())
                    .ok_or_else(invalid)?;
                self.identity(response)?;
                if let Some(output) = response.get("output").filter(|v| !v.is_null()) {
                    let output = output
                        .as_array()
                        .filter(|v| v.len() <= 1024)
                        .ok_or_else(invalid)?;
                    for (index, item) in output.iter().enumerate() {
                        self.item_identity(index as u32, item, "id")?;
                    }
                }
                if self.response.is_some()
                    || self.retained.saturating_add(size(response)?) > crate::MAX_STREAM_LINE_BYTES
                {
                    return Err(invalid());
                }
                self.retained += size(response)?;
                self.response = Some(response.clone());
            }
            Some(
                "response.output_text.delta"
                | "response.output_text.done"
                | "response.refusal.delta"
                | "response.refusal.done"
                | "response.reasoning_summary_text.delta"
                | "response.reasoning_summary_text.done"
                | "response.reasoning_text.delta"
                | "response.reasoning_text.done"
                | "response.content_part.added"
                | "response.content_part.done"
                | "response.reasoning_summary_part.added"
                | "response.reasoning_summary_part.done"
                | "response.function_call_arguments.delta"
                | "response.function_call_arguments.done",
            ) => self.materialize(event)?,
            _ if self.validate_materialized => return Err(invalid()),
            _ => {}
        }
        Ok(())
    }
    pub(super) fn finish(self) -> Result<Option<Value>, ProviderError> {
        let Some(mut response) = self.response else {
            return if self.items.is_empty() {
                Ok(None)
            } else {
                Err(invalid())
            };
        };
        if self.validate_materialized {
            for (key, prior) in [("id", &self.response_id), ("model", &self.model)] {
                if prior
                    .as_deref()
                    .is_some_and(|prior| response.get(key).and_then(Value::as_str) != Some(prior))
                {
                    return Err(invalid());
                }
            }
        }
        if self.items.is_empty() && self.materialized.is_empty() {
            return Ok(Some(response));
        }
        let output = response
            .get_mut("output")
            .and_then(Value::as_array_mut)
            .ok_or_else(invalid)?;
        for (index, (item, done, _)) in self.items {
            let index = index as usize;
            if let Some(terminal) = output.get(index) {
                if if done {
                    terminal != &item
                } else {
                    !covers(&item, terminal, "")
                } {
                    return Err(invalid());
                }
            } else if done && index == output.len() {
                output.push(item);
            } else {
                // Neither a partial item nor a hole is a complete replay input.
                return Err(invalid());
            }
        }
        for (index, id) in self.item_ids {
            if output
                .get(index as usize)
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
                != Some(id.as_str())
            {
                return Err(invalid());
            }
        }
        for ((index, part, channel), (value, complete)) in self.materialized {
            let item = output.get(index as usize).ok_or_else(invalid)?;
            let actual = if channel == 4 {
                item.get("arguments").and_then(Value::as_str)
            } else {
                let part = item
                    .get(if channel == 2 { "summary" } else { "content" })
                    .and_then(Value::as_array)
                    .and_then(|parts| parts.get(part as usize))
                    .ok_or_else(invalid)?;
                if part.get("type").and_then(Value::as_str)
                    != Some(match channel {
                        0 => "output_text",
                        1 => "refusal",
                        2 => "summary_text",
                        _ => "reasoning_text",
                    })
                {
                    return Err(invalid());
                }
                part.get(if channel == 1 { "refusal" } else { "text" })
                    .and_then(Value::as_str)
            }
            .ok_or_else(invalid)?;
            if if complete {
                actual != value
            } else {
                !actual.starts_with(&value)
            } {
                return Err(invalid());
            }
        }
        Ok(Some(response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn incomplete_native_state_is_never_promoted_to_a_complete_item() {
        let added = json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs","summary":[],"encrypted_content":"partial"}});
        let mut state = NativeAggregate::for_delivery(false);
        state.observe(&added).unwrap();
        state
            .observe(&json!({"type":"response.completed","response":{"output":[]}}))
            .unwrap();
        assert!(state.finish().is_err());
        let mut state = NativeAggregate::for_delivery(false);
        state.observe(&added).unwrap();
        state.observe(&json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs","summary":[],"encrypted_content":"complete"}})).unwrap();
        state
            .observe(&json!({"type":"response.completed","response":{"output":[]}}))
            .unwrap();
        assert_eq!(
            state.finish().unwrap().unwrap()["output"][0]["encrypted_content"],
            "complete"
        );
    }
    #[test]
    fn progress_only_native_items_and_uncovered_content_fail_closed() {
        let mut state = NativeAggregate::for_delivery(false);
        state.observe(&json!({"type":"response.in_progress","response":{"output":[{"type":"reasoning","encrypted_content":"partial"}]}})).unwrap();
        state
            .observe(&json!({"type":"response.completed","response":{"output":[]}}))
            .unwrap();
        assert!(state.finish().is_err());
        let mut state = NativeAggregate::for_delivery(false);
        state
            .observe(&json!({"type":"response.output_text.delta","delta":"reply"}))
            .unwrap();
        state
            .observe(&json!({"type":"response.completed","response":{"output":[]}}))
            .unwrap();
        assert!(state.finish().is_err());
        let mut state = NativeAggregate::for_delivery(false);
        assert!(state.observe(&json!({"type":"response.output_text.annotation.added","annotation":{"url":"https://example.invalid"}})).is_err());
        for event in [
            json!({"type":"response.output_text.delta","delta":"reply","logprobs":[{"token":"reply"}]}),
            json!({"type":"response.output_text.done","text":"reply","obfuscation":{"state":"opaque"}}),
        ] {
            let mut state = NativeAggregate::for_delivery(false);
            assert!(state.observe(&event).is_err());
            assert!(state.materialized.is_empty());
        }
    }
    #[test]
    fn native_retention_preflights_clones_and_rejects_output_holes() {
        let mut state = NativeAggregate {
            retained: crate::MAX_STREAM_LINE_BYTES,
            ..NativeAggregate::for_delivery(false)
        };
        assert!(state.observe(&json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"complete"}})).is_err());
        assert!(state.items.is_empty());
        assert!(state
            .observe(&json!({"type":"response.completed","response":{"output":[]}}))
            .is_err());
        assert!(state.response.is_none());
        assert!(state
            .observe(&json!({"type":"response.output_text.delta","delta":"x"}))
            .is_err());
        assert!(state.materialized.is_empty());
        let mut state = NativeAggregate::for_delivery(false);
        state.observe(&json!({"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","encrypted_content":"complete"}})).unwrap();
        state
            .observe(&json!({"type":"response.completed","response":{"output":[]}}))
            .unwrap();
        assert!(state.finish().is_err());
    }
    #[test]
    fn materialized_item_ids_must_agree_and_terminal_bytes_stay_accounted() {
        let mut state = NativeAggregate::for_delivery(false);
        state.observe(&json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_a","content":[]}})).unwrap();
        assert!(state.observe(&json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_b","delta":"reply"})).is_err());
        assert!(state.materialized.is_empty());
        let mut state = NativeAggregate::for_delivery(false);
        state.observe(&json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_a","delta":"reply"})).unwrap();
        assert!(state.observe(&json!({"type":"response.completed","response":{"output":[{"type":"message","id":"msg_b","content":[{"type":"output_text","text":"reply"}]}]}})).is_err());
        let response = json!({"output":[]});
        let mut state = NativeAggregate::for_delivery(false);
        state
            .observe(&json!({"type":"response.completed","response":response}))
            .unwrap();
        assert_eq!(state.retained, size(&response).unwrap());
        let retained = state.retained;
        assert!(state
            .observe(&json!({"type":"response.output_text.delta","delta":"after"}))
            .is_err());
        assert_eq!(state.retained, retained);
        let mut state = NativeAggregate::for_delivery(false);
        state.observe(&json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg_a","delta":"reply"})).unwrap();
        state.observe(&json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"reply"}]}]}})).unwrap();
        assert!(state.finish().is_err());
        let mut state = NativeAggregate::for_delivery(false);
        state.observe(&json!({"type":"response.created","response":{"id":"resp_a","model":"fixture","output":[]}})).unwrap();
        state
            .observe(&json!({"type":"response.completed","response":{"output":[]}}))
            .unwrap();
        assert!(state.finish().is_err());
    }
}
