//! Portable complete-event Responses output over the existing bounded source.
use crate::buffered_fidelity::{array, fields, optional_role, string_args, text};
use crate::{ByteStream, ChatEventStream, ProviderError};
use futures_util::StreamExt;
use sandhi_core::{ChatStreamEventV1 as Event, EndpointFamilyV1, FinishReasonV1};
use serde_json::Value;
use std::collections::BTreeMap;

fn invalid() -> ProviderError {
    ProviderError::OutputProjection("Responses output has nonportable or inconsistent content; compatible native delivery may be required".into())
}
pub(crate) fn decode(raw: ByteStream, model: String, capture: bool) -> ChatEventStream {
    crate::cache_read::decode_with_observation(raw, model, move |raw, model| {
        decode_inner(raw, model, capture)
    })
}
fn decode_inner(mut raw: ByteStream, model: String, capture: bool) -> ChatEventStream {
    Box::pin(async_stream::try_stream! {
        let mut framing=crate::native_stream::SseDecoder::new(Some(EndpointFamilyV1::OpenaiResponses));
        let mut projection=Projection::default();
        let mut started=false;
        let mut failure=None;
        while let Some(chunk)=raw.next().await {
            let chunk=chunk?;
            // Projection has no transport owner. Drain the SAME bounded source
            // on refusal so actual counters and transport EOF remain observable.
            if failure.is_some() {continue;}
            framing.push(&chunk.data);
            loop {
                let (line,frame)=match framing.next_line() {
                    Ok(Some(event))=>event,Ok(None)=>break,Err(error)=>{failure=Some(error);break;}
                };
                let Some(data)=line.strip_prefix(b"data:") else {continue;};
                let data=data.strip_prefix(b" ").unwrap_or(data);
                let value=match sandhi_core::policy::strict_json(data) {Ok(value)=>value,Err(_)=>{failure=Some(invalid());break;}};
                match projection.event(&value) {
                    Ok(events)=>{
                        if !started {
                            started=true;
                            yield Event::ResponseStart{id:projection.response_id.clone(),model:projection.model.clone().unwrap_or_else(||model.clone())};
                        }
                        for event in events {yield event;}
                        if capture {if let Some(frame)=frame {yield Event::NativeFrame{frame};}}
                    }
                    Err(error)=>{failure=Some(error);break;}
                }
            }
            if framing.over_budget() {failure.get_or_insert_with(invalid);}
        }
        if let Some(error)=failure {Err(error)?;}
        yield Event::Finish{reason:projection.finish.ok_or_else(invalid)?};
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Message,
    Reasoning,
    Call,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Channel {
    Text,
    Refusal,
    Summary,
    Reasoning,
}
enum Update {
    Delta,
    Complete,
    Snapshot { complete: bool, announced: bool },
}
#[derive(Default)]
struct Part {
    value: String,
    emitted: usize,
    closed: bool,
    announced: bool,
}
struct Item {
    kind: Kind,
    id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    parts: BTreeMap<(u32, Channel), Part>,
    announced: bool,
    closed: bool,
    done_seen: bool,
    args_done: bool,
    call_emitted: bool,
}
impl Item {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            id: None,
            call_id: None,
            name: None,
            arguments: String::new(),
            parts: BTreeMap::new(),
            announced: false,
            closed: false,
            done_seen: false,
            args_done: false,
            call_emitted: false,
        }
    }
}
#[derive(Default)]
struct Projection {
    items: BTreeMap<u32, Item>,
    retained: usize,
    parts: usize,
    sequence: Option<u64>,
    response_id: Option<String>,
    model: Option<String>,
    finish: Option<FinishReasonV1>,
    last_emitted: Option<(u32, u32, Channel)>,
}
fn number(value: &Value, key: &str) -> Result<Option<u32>, ProviderError> {
    value
        .get(key)
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(invalid)
        })
        .transpose()
}
fn optional<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, ProviderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(v)) => Ok(Some(v)),
        _ => Err(invalid()),
    }
}
fn same(
    slot: &mut Option<String>,
    value: Option<&str>,
    retained: &mut usize,
) -> Result<(), ProviderError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    if let Some(previous) = slot {
        if previous != value {
            return Err(invalid());
        }
    } else {
        reserve(retained, value.len())?;
        *slot = Some(value.to_owned());
    }
    Ok(())
}
fn reserve(retained: &mut usize, extra: usize) -> Result<(), ProviderError> {
    let next = retained.saturating_add(extra);
    if next > crate::MAX_STREAM_LINE_BYTES {
        return Err(invalid());
    }
    *retained = next;
    Ok(())
}
fn append(
    target: &mut String,
    value: &str,
    complete: bool,
    retained: &mut usize,
) -> Result<(), ProviderError> {
    if complete {
        if !value.starts_with(target.as_str()) {
            return Err(invalid());
        }
        reserve(retained, value.len() - target.len())?;
        target.push_str(&value[target.len()..]);
    } else {
        reserve(retained, value.len())?;
        target.push_str(value);
    }
    Ok(())
}
impl Projection {
    fn ensure(
        &mut self,
        index: u32,
        kind: Kind,
        identity: Option<&str>,
    ) -> Result<(), ProviderError> {
        if !self.items.contains_key(&index) {
            if self.items.len() >= 1024 || self.last_emitted.is_some_and(|(i, _, _)| index < i) {
                return Err(invalid());
            }
            self.items.insert(index, Item::new(kind));
        }
        if identity.is_some_and(|id| {
            self.items
                .iter()
                .any(|(i, item)| *i != index && item.id.as_deref() == Some(id))
        }) {
            return Err(invalid());
        }
        let item = self.items.get_mut(&index).unwrap();
        if item.kind != kind {
            return Err(invalid());
        }
        same(&mut item.id, identity, &mut self.retained)
    }
    fn part(
        &mut self,
        index: u32,
        channel: Channel,
        part: u32,
        value: &str,
        update: Update,
    ) -> Result<(), ProviderError> {
        let (complete, announced, snapshot) = match update {
            Update::Delta => (false, false, false),
            Update::Complete => (true, false, false),
            Update::Snapshot {
                complete,
                announced,
            } => (complete, announced, true),
        };
        let kind = match channel {
            Channel::Text | Channel::Refusal => Kind::Message,
            _ => Kind::Reasoning,
        };
        self.ensure(index, kind, None)?;
        let key = (part, channel);
        let item = self.items.get_mut(&index).unwrap();
        if item
            .parts
            .keys()
            .any(|(i, c)| *i == part && *c != channel && kind == Kind::Message)
        {
            return Err(invalid());
        }
        if !item.parts.contains_key(&key) {
            if (item.closed && !value.is_empty())
                || self.parts >= 1024
                || (!value.is_empty()
                    && self
                        .last_emitted
                        .is_some_and(|previous| (index, part, channel) < previous))
            {
                return Err(invalid());
            }
            self.parts += 1;
            item.parts.insert(key, Part::default());
        }
        let target = item.parts.get_mut(&key).unwrap();
        if (announced && target.announced) || (target.closed && !complete && !value.is_empty()) {
            return Err(invalid());
        }
        if complete && target.closed && !value.is_empty() && value != target.value {
            return Err(invalid());
        }
        if !(announced && value.is_empty()) {
            append(
                &mut target.value,
                value,
                complete || snapshot,
                &mut self.retained,
            )?;
        }
        target.announced |= announced;
        target.closed |= complete;
        Ok(())
    }
    fn snapshot_part(
        &mut self,
        index: u32,
        part: u32,
        value: &Value,
        complete: bool,
        announced: bool,
        summary: bool,
    ) -> Result<(), ProviderError> {
        let channel = match text(value, "type")? {
            "output_text" if !summary => Channel::Text,
            "refusal" if !summary => Channel::Refusal,
            "summary_text" if summary => Channel::Summary,
            "reasoning_text" if !summary => Channel::Reasoning,
            _ => return Err(invalid()),
        };
        fields(
            value,
            &[
                "type",
                if channel == Channel::Refusal {
                    "refusal"
                } else {
                    "text"
                },
            ],
        )?;
        let content = text(
            value,
            if channel == Channel::Refusal {
                "refusal"
            } else {
                "text"
            },
        )?;
        self.part(
            index,
            channel,
            part,
            content,
            Update::Snapshot {
                complete,
                announced,
            },
        )
    }
    fn snapshot_item(
        &mut self,
        index: u32,
        value: &Value,
        complete: bool,
        announced: bool,
    ) -> Result<(), ProviderError> {
        let kind = match text(value, "type")? {
            "message" => Kind::Message,
            "reasoning" => Kind::Reasoning,
            "function_call" => Kind::Call,
            _ => return Err(invalid()),
        };
        if optional(value, "status")?
            .is_some_and(|status| !matches!(status, "in_progress" | "completed" | "incomplete"))
        {
            return Err(invalid());
        }
        self.ensure(index, kind, optional(value, "id")?)?;
        if complete {
            // A complete item must cover every retained meaningful part. An
            // empty aggregate output may be a sparse provider envelope, but an
            // explicit complete message/reasoning item cannot erase its parts.
            for ((part, channel), prior) in &self.items[&index].parts {
                if prior.value.is_empty() {
                    continue;
                }
                let key = if *channel == Channel::Summary {
                    "summary"
                } else {
                    "content"
                };
                let parts = value
                    .get(key)
                    .and_then(Value::as_array)
                    .ok_or_else(invalid)?;
                let part = parts.get(*part as usize).ok_or_else(invalid)?;
                let expected = match channel {
                    Channel::Text => "output_text",
                    Channel::Refusal => "refusal",
                    Channel::Summary => "summary_text",
                    Channel::Reasoning => "reasoning_text",
                };
                if text(part, "type")? != expected {
                    return Err(invalid());
                }
            }
            if kind == Kind::Call {
                text(value, "arguments")?;
            }
        }
        if announced && (self.items[&index].announced || self.items[&index].closed) {
            return Err(invalid());
        }
        match kind {
            Kind::Message => {
                fields(value, &["type", "id", "role", "status", "content"])?;
                optional_role(value, "role", "assistant")?;
                if let Some(parts) = value.get("content").filter(|v| !v.is_null()) {
                    let parts = parts
                        .as_array()
                        .filter(|p| p.len() <= 1024)
                        .ok_or_else(invalid)?;
                    for (i, part) in parts.iter().enumerate() {
                        self.snapshot_part(index, i as u32, part, complete, false, false)?;
                    }
                }
            }
            Kind::Reasoning => {
                fields(value, &["type", "id", "status", "summary", "content"])?;
                for (key, summary) in [("summary", true), ("content", false)] {
                    if value.get(key).is_some_and(|v| !v.is_null()) {
                        for (i, part) in array(value, key)?.iter().enumerate() {
                            self.snapshot_part(index, i as u32, part, complete, false, summary)?;
                        }
                    }
                }
            }
            Kind::Call => {
                fields(
                    value,
                    &["type", "id", "call_id", "name", "arguments", "status"],
                )?;
                let call_id = optional(value, "call_id")?;
                if call_id.is_some_and(|id| {
                    self.items
                        .iter()
                        .any(|(i, v)| *i != index && v.call_id.as_deref() == Some(id))
                }) {
                    return Err(invalid());
                }
                let item = self.items.get_mut(&index).unwrap();
                same(&mut item.call_id, call_id, &mut self.retained)?;
                same(&mut item.name, optional(value, "name")?, &mut self.retained)?;
                if let Some(arguments) = optional(value, "arguments")? {
                    if (item.args_done || item.closed || item.call_emitted)
                        && arguments != item.arguments
                    {
                        return Err(invalid());
                    }
                    // Item snapshots are complete prefixes, never another delta.
                    if !(announced && arguments.is_empty()) {
                        append(&mut item.arguments, arguments, true, &mut self.retained)?;
                    }
                }
            }
        }
        let item = self.items.get_mut(&index).unwrap();
        item.announced |= announced;
        item.closed |= complete;
        if complete {
            for part in item.parts.values_mut() {
                part.closed = true;
            }
        }
        Ok(())
    }
    fn response(&mut self, value: &Value, terminal: bool) -> Result<(), ProviderError> {
        fields(value, crate::buffered_fidelity::RESPONSES_FIELDS)?;
        same(
            &mut self.response_id,
            optional(value, "id")?,
            &mut self.retained,
        )?;
        same(
            &mut self.model,
            optional(value, "model")?,
            &mut self.retained,
        )?;
        if value.get("output").is_some_and(|v| !v.is_null()) {
            let output = array(value, "output")?;
            if terminal
                && !output.is_empty()
                && self.items.iter().any(|(index, item)| {
                    (*index as usize) >= output.len()
                        && (!item.arguments.is_empty()
                            || item.parts.values().any(|part| !part.value.is_empty()))
                })
            {
                return Err(invalid());
            }
            for (i, item) in output.iter().enumerate() {
                self.snapshot_item(i as u32, item, terminal, false)?;
            }
        }
        if terminal {
            if !matches!(text(value, "status")?, "completed" | "incomplete") {
                return Err(invalid());
            }
            if text(value, "status")? == "incomplete" {
                let details = value.get("incomplete_details").ok_or_else(invalid)?;
                fields(details, &["reason"])?;
                self.finish = Some(match text(details, "reason")? {
                    "max_output_tokens" => FinishReasonV1::Length,
                    "content_filter" => FinishReasonV1::ContentFilter,
                    _ => return Err(invalid()),
                });
            } else {
                if value
                    .get("incomplete_details")
                    .is_some_and(|v| !v.is_null())
                {
                    return Err(invalid());
                }
                self.finish = Some(if self.items.values().any(|v| v.kind == Kind::Call) {
                    FinishReasonV1::ToolCalls
                } else {
                    FinishReasonV1::Stop
                });
            }
            for item in self.items.values_mut() {
                item.closed = true;
                for part in item.parts.values_mut() {
                    part.closed = true;
                }
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<Vec<Event>, ProviderError> {
        let mut events = vec![];
        for (index, item) in &mut self.items {
            if item.kind == Kind::Call {
                if !item.closed {
                    break;
                }
                if item.call_emitted {
                    continue;
                }
                let (Some(call_id), Some(name)) = (&item.call_id, &item.name) else {
                    return Err(invalid());
                };
                string_args(&Value::String(item.arguments.clone()))?;
                events.push(Event::ToolCallStart {
                    index: *index,
                    id: call_id.clone(),
                    name: name.clone(),
                });
                events.push(Event::ToolCallArgumentsDelta {
                    index: *index,
                    delta: item.arguments.clone(),
                });
                events.push(Event::ToolCallEnd { index: *index });
                item.call_emitted = true;
                self.last_emitted = Some((*index, 0, Channel::Text));
            } else {
                for ((part_index, channel), part) in &mut item.parts {
                    if part.emitted < part.value.len() {
                        if self
                            .last_emitted
                            .is_some_and(|previous| (*index, *part_index, *channel) < previous)
                        {
                            return Err(invalid());
                        }
                        let delta = part.value[part.emitted..].to_owned();
                        part.emitted = part.value.len();
                        events.push(match channel {
                            Channel::Text => Event::TextDelta { delta },
                            Channel::Refusal => Event::RefusalDelta { delta },
                            _ => Event::ReasoningDelta { delta },
                        });
                        self.last_emitted = Some((*index, *part_index, *channel));
                    }
                    if !part.closed {
                        break;
                    }
                }
                if !item.closed {
                    break;
                }
            }
        }
        Ok(events)
    }
    fn event(&mut self, event: &Value) -> Result<Vec<Event>, ProviderError> {
        if self.finish.is_some() {
            return Err(invalid());
        }
        fields(
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
        )?;
        if event
            .get("obfuscation")
            .is_some_and(|v| !v.is_null() && !v.is_string())
        {
            return Err(invalid());
        }
        if let Some(sequence) = event.get("sequence_number") {
            let sequence = sequence.as_u64().ok_or_else(invalid)?;
            if self.sequence.is_some_and(|prior| sequence <= prior) {
                return Err(invalid());
            }
            self.sequence = Some(sequence);
        }
        same(
            &mut self.response_id,
            optional(event, "response_id")?,
            &mut self.retained,
        )?;
        let kind = text(event, "type")?;
        let index = number(event, "output_index")?.unwrap_or(0);
        match kind {
            "response.created" | "response.in_progress" | "response.queued" => {
                self.response(event.get("response").ok_or_else(invalid)?, false)?
            }
            "response.completed" | "response.incomplete" => {
                let response = event.get("response").ok_or_else(invalid)?;
                if text(response, "status")? != kind.strip_prefix("response.").unwrap() {
                    return Err(invalid());
                }
                self.response(response, true)?
            }
            "response.output_item.added" | "response.output_item.done" => {
                if kind.ends_with(".done") && self.items.get(&index).is_some_and(|v| v.done_seen) {
                    return Err(invalid());
                }
                self.snapshot_item(
                    index,
                    event.get("item").ok_or_else(invalid)?,
                    kind.ends_with(".done"),
                    kind.ends_with(".added"),
                )?;
                if kind.ends_with(".done") {
                    self.items.get_mut(&index).unwrap().done_seen = true;
                }
            }
            "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done" => {
                let summary = kind.contains("reasoning_summary");
                let part = number(
                    event,
                    if summary {
                        "summary_index"
                    } else {
                        "content_index"
                    },
                )?
                .unwrap_or(0);
                self.snapshot_part(
                    index,
                    part,
                    event.get("part").ok_or_else(invalid)?,
                    kind.ends_with(".done"),
                    kind.ends_with(".added"),
                    summary,
                )?;
                let item_kind = self.items[&index].kind;
                self.ensure(index, item_kind, optional(event, "item_id")?)?;
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done" => {
                let channel = if kind.contains("output_text") {
                    Channel::Text
                } else if kind.contains("refusal") {
                    Channel::Refusal
                } else if kind.contains("reasoning_summary") {
                    Channel::Summary
                } else {
                    Channel::Reasoning
                };
                let complete = kind.ends_with(".done");
                let key = if !complete {
                    "delta"
                } else if channel == Channel::Refusal {
                    "refusal"
                } else {
                    "text"
                };
                let part = number(
                    event,
                    if channel == Channel::Summary {
                        "summary_index"
                    } else {
                        "content_index"
                    },
                )?
                .unwrap_or(0);
                self.part(
                    index,
                    channel,
                    part,
                    text(event, key)?,
                    if complete {
                        Update::Complete
                    } else {
                        Update::Delta
                    },
                )?;
                let item_kind = self.items[&index].kind;
                self.ensure(index, item_kind, optional(event, "item_id")?)?;
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                self.ensure(index, Kind::Call, optional(event, "item_id")?)?;
                let item = self.items.get_mut(&index).unwrap();
                let complete = kind.ends_with(".done");
                let arguments = text(event, if complete { "arguments" } else { "delta" })?;
                if item.closed || (item.args_done && (!arguments.is_empty() || complete)) {
                    return Err(invalid());
                }
                append(&mut item.arguments, arguments, complete, &mut self.retained)?;
                item.args_done |= complete;
            }
            "response.failed" | "error" => {
                return Err(ProviderError::Transport(
                    "Responses upstream reported failure".into(),
                ))
            }
            _ => return Err(invalid()),
        }
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn terminal() -> Value {
        json!({"type":"response.completed","response":{"status":"completed","output":[]}})
    }
    fn call(index: u32, done: bool) -> Value {
        json!({"type":if done {"response.output_item.done"}else{"response.output_item.added"},"output_index":index,
            "item":{"type":"function_call","id":format!("fc_{index}"),"call_id":format!("call_{index}"),"name":"inspect","arguments":if done {"{}"}else{""}}})
    }
    #[test]
    fn parallel_calls_complete_in_output_order_and_text_keeps_its_position() {
        let mut projection = Projection::default();
        assert!(projection.event(&call(0, false)).unwrap().is_empty());
        assert!(projection.event(&call(1, false)).unwrap().is_empty());
        assert!(projection.event(&call(1, true)).unwrap().is_empty());
        let events = projection.event(&call(0, true)).unwrap();
        let indices: Vec<_> = events
            .iter()
            .filter_map(|v| match v {
                Event::ToolCallStart { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, [0, 1]);
        let events = projection.event(&json!({"type":"response.output_text.delta","output_index":2,"content_index":0,"delta":"after"})).unwrap();
        assert!(matches!(&events[..], [Event::TextDelta{delta}] if delta=="after"));
        projection.event(&terminal()).unwrap();
        assert!(projection.event(&terminal()).is_err());
    }
    #[test]
    fn completed_calls_cannot_change_arguments_or_report_a_failed_item() {
        let mut projection = Projection::default();
        projection.event(&call(0, true)).unwrap();
        let mut changed = call(0, true)["item"].clone();
        changed["arguments"] = "{} ".into();
        assert!(projection.event(&serde_json::json!({"type":"response.completed","response":{"status":"completed","output":[changed]}})).is_err());
        let mut projection = Projection::default();
        let mut failed = call(0, true);
        failed["item"]["status"] = "failed".into();
        assert!(projection.event(&failed).is_err());
    }
    #[test]
    fn complete_item_snapshots_must_cover_previously_emitted_parts() {
        for (delta, item) in [
            (
                json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"reply"}),
                json!({"type":"message","role":"assistant","content":[]}),
            ),
            (
                json!({"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"summary"}),
                json!({"type":"reasoning","summary":[]}),
            ),
        ] {
            for terminal in [false, true] {
                let mut projection = Projection::default();
                projection.event(&delta).unwrap();
                let complete = if terminal {
                    json!({"type":"response.completed","response":{"status":"completed","output":[item]}})
                } else {
                    json!({"type":"response.output_item.done","output_index":0,"item":item})
                };
                assert!(
                    projection.event(&complete).is_err(),
                    "accepted omitted content: {complete}"
                );
            }
        }
    }
    #[test]
    fn part_index_precedes_channel_and_progress_snapshots_do_not_duplicate_text() {
        let mut projection = Projection::default();
        let mut body = json!({"status":"in_progress","output":[{"type":"message","id":"msg","role":"assistant","content":[{"type":"refusal","refusal":"no"},{"type":"output_text","text":"why"}]}]});
        let first = projection
            .event(&json!({"type":"response.in_progress","response":body}))
            .unwrap();
        assert!(matches!(&first[..], [Event::RefusalDelta{delta}] if delta=="no"));
        assert!(projection
            .event(&json!({"type":"response.in_progress","response":body}))
            .unwrap()
            .is_empty());
        body["status"] = "completed".into();
        let events = projection
            .event(&json!({"type":"response.completed","response":body}))
            .unwrap();
        assert!(matches!(&events[..], [Event::TextDelta{delta}] if delta=="why"));
    }
    #[test]
    fn identity_lifecycle_and_meaningful_native_fields_refuse() {
        let initial = json!({"type":"response.output_text.delta","item_id":"msg","output_index":0,"content_index":0,"delta":"a"});
        for bad in [
            json!({"type":"response.output_text.delta","item_id":"different","output_index":0,"delta":"b"}),
            json!({"type":"response.output_text.delta","item_id":"msg","output_index":1,"delta":"b"}),
            json!({"type":"response.output_text.delta","output_index":4294967296_u64,"delta":"b"}),
            json!({"type":"response.output_text.delta","output_index":-1,"delta":"b"}),
            json!({"type":"response.output_text.delta","delta":"b","obfuscation":{}}),
            json!({"type":"response.output_text.done","text":"different"}),
            json!({"type":"response.refusal.delta","content_index":0,"delta":"b"}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","encrypted_content":"opaque"}}),
            json!({"type":"response.output_text.annotation.added","annotation":{"url":"https://example.invalid"}}),
            json!({"type":"response.output_text.delta","delta":"b","logprobs":[{"token":"b"}]}),
        ] {
            let mut projection = Projection::default();
            projection.event(&initial).unwrap();
            assert!(projection.event(&bad).is_err(), "accepted {bad}");
        }
        let mut projection = Projection::default();
        projection
            .event(&json!({"type":"response.output_text.done","text":"a"}))
            .unwrap();
        assert!(projection.event(&initial).is_err());
        let mut projection = Projection::default();
        projection.event(&json!({"type":"response.created","sequence_number":3,"response":{"id":"a","model":"m"}})).unwrap();
        assert!(projection
            .event(&json!({"type":"response.in_progress","sequence_number":3,"response":{}}))
            .is_err());
        assert!(projection
            .event(&json!({"type":"response.in_progress","response":{"id":"b"}}))
            .is_err());
    }
    #[test]
    fn retention_limits_are_checked_before_storing_rejected_fragments() {
        let mut projection = Projection {
            retained: crate::MAX_STREAM_LINE_BYTES,
            ..Projection::default()
        };
        assert!(projection
            .event(&json!({"type":"response.created","response":{"id":"new"}}))
            .is_err());
        assert!(projection.response_id.is_none());
        let mut projection = Projection::default();
        projection
            .event(&json!({"type":"response.output_text.delta","delta":"ab"}))
            .unwrap();
        projection.retained = crate::MAX_STREAM_LINE_BYTES;
        assert!(projection
            .event(&json!({"type":"response.output_text.delta","delta":"cd"}))
            .is_err());
        assert_eq!(projection.items[&0].parts[&(0, Channel::Text)].value, "ab");
        let mut projection = Projection::default();
        projection.event(&call(0, false)).unwrap();
        projection.retained = crate::MAX_STREAM_LINE_BYTES;
        assert!(projection
            .event(&json!({"type":"response.function_call_arguments.delta","delta":"{}"}))
            .is_err());
        assert!(projection.items[&0].arguments.is_empty());
        let mut projection = Projection::default();
        for index in 0..1024 {
            projection.ensure(index, Kind::Message, None).unwrap();
        }
        assert!(projection.ensure(1024, Kind::Message, None).is_err());
        assert_eq!(projection.items.len(), 1024);
    }
    #[tokio::test]
    async fn complete_lifecycle_is_invariant_at_every_multiline_byte_split() {
        let frames = vec![
            json!({"type":"response.created","sequence_number":0,"response":{"id":"resp","model":"m","output":[]}}),
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg","role":"assistant","content":[]}}),
            json!({"type":"response.content_part.added","sequence_number":2,"output_index":0,"item_id":"msg","content_index":0,"part":{"type":"output_text","text":"","annotations":[],"logprobs":[]}}),
            json!({"type":"response.output_text.delta","sequence_number":3,"output_index":0,"item_id":"msg","content_index":0,"delta":"hé🙂","obfuscation":"padding"}),
            json!({"type":"response.output_text.done","sequence_number":4,"output_index":0,"item_id":"msg","content_index":0,"text":"hé🙂"}),
            json!({"type":"response.content_part.done","sequence_number":5,"output_index":0,"item_id":"msg","content_index":0,"part":{"type":"output_text","text":"hé🙂"}}),
            json!({"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hé🙂"}]}}),
            json!({"type":"response.completed","sequence_number":7,"response":{"id":"resp","model":"m","status":"completed","output":[{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"hé🙂"}]}]}}),
        ];
        for multiline in [false, true] {
            let wire = frames
                .iter()
                .map(|v| {
                    let body = if multiline {
                        serde_json::to_string_pretty(v).unwrap()
                    } else {
                        v.to_string()
                    };
                    format!(
                        "event: {}\r\n{}\r\n",
                        v["type"].as_str().unwrap(),
                        body.lines()
                            .map(|l| format!("data: {l}\r\n"))
                            .collect::<String>()
                    )
                })
                .collect::<String>();
            for split in 0..=wire.len() {
                let raw = Box::pin(futures_util::stream::iter(vec![
                    Ok(crate::StreamChunk {
                        data: bytes::Bytes::copy_from_slice(&wire.as_bytes()[..split]),
                        ..Default::default()
                    }),
                    Ok(crate::StreamChunk {
                        data: bytes::Bytes::copy_from_slice(&wire.as_bytes()[split..]),
                        ..Default::default()
                    }),
                ]));
                let events = decode(raw, "fallback".into(), true)
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap_or_else(|e| panic!("{multiline}/{split}: {e}"));
                assert_eq!(
                    events
                        .iter()
                        .filter_map(|v| match v {
                            Event::TextDelta { delta } => Some(delta.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    "hé🙂"
                );
                assert!(matches!(
                    events.last(),
                    Some(Event::Finish {
                        reason: FinishReasonV1::Stop
                    })
                ));
                assert_eq!(
                    events
                        .into_iter()
                        .filter_map(|v| match v {
                            Event::NativeFrame { frame } => frame.json_data(),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    frames
                );
            }
        }
    }
}
