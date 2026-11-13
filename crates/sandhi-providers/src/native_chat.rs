//! Portable native Cohere/Ollama projection. Counts belong exclusively to the
//! response-owned observer. A projection failure drains the same bounded source.
use crate::{buffered_fidelity as f, ChatEventStream, ProviderError};
use futures_util::StreamExt;
use sandhi_core::{
    ChatResponseV1, ChatStreamEventV1 as Event, EndpointFamilyV1 as Family,
    FinishReasonV1 as Finish,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
fn invalid() -> ProviderError {
    ProviderError::OutputProjection(
        "native chat output has unsupported content or invalid lifecycle".into(),
    )
}
fn nonempty(value: &Value, key: &str) -> Result<String, ProviderError> {
    Ok(f::id(value, key)?.into())
}
fn reason(family: Family, value: &str, tools: bool) -> Result<Finish, ProviderError> {
    Ok(match (family, value) {
        (Family::CohereChat, "COMPLETE" | "STOP_SEQUENCE") | (Family::OllamaChat, "stop") => {
            if tools {
                Finish::ToolCalls
            } else {
                Finish::Stop
            }
        }
        (Family::CohereChat, "TOOL_CALL") if tools => Finish::ToolCalls,
        (Family::CohereChat, "MAX_TOKENS") | (Family::OllamaChat, "length") => Finish::Length,
        _ => return Err(invalid()),
    })
}
fn calls(value: Option<&Value>, family: Family) -> Result<(), ProviderError> {
    let Some(value) = value else { return Ok(()) };
    let values = value
        .as_array()
        .filter(|v| v.len() <= 1024)
        .ok_or_else(invalid)?;
    let mut ids = BTreeSet::new();
    for call in values {
        f::fields(call, &["id", "type", "function"])?;
        if call
            .get("type")
            .is_some_and(|v| v.as_str() != Some("function"))
        {
            return Err(invalid());
        }
        if (family == Family::CohereChat || call.get("id").is_some())
            && !ids.insert(nonempty(call, "id")?)
        {
            return Err(invalid());
        }
        let function = call.get("function").ok_or_else(invalid)?;
        f::fields(function, &["name", "arguments", "index"])?;
        nonempty(function, "name")?;
        if function
            .get("index")
            .is_some_and(|v| !v.as_u64().is_some_and(|v| v < 1024))
        {
            return Err(invalid());
        }
        if family == Family::CohereChat {
            f::string_args(function.get("arguments").ok_or_else(invalid)?)?;
        } else if !function.get("arguments").is_some_and(Value::is_object) {
            return Err(invalid());
        }
    }
    Ok(())
}
pub(crate) fn validate_buffered(family: Family, body: &Value) -> Result<(), ProviderError> {
    if family == Family::CohereChat {
        f::fields(body, &["id", "message", "finish_reason", "usage"])?;
        let message = body.get("message").ok_or_else(invalid)?;
        f::fields(message, &["role", "content", "tool_calls", "tool_plan"])?;
        f::optional_text(message, "tool_plan")?;
        f::optional_role(message, "role", "assistant")?;
        if let Some(content) = message.get("content") {
            for part in content.as_array().ok_or_else(invalid)? {
                f::fields(part, &["type", "text"])?;
                if part.get("type").is_some_and(|v| v.as_str() != Some("text")) {
                    return Err(invalid());
                }
                f::text(part, "text")?;
            }
        }
        calls(message.get("tool_calls"), family)?;
        reason(
            family,
            f::text(body, "finish_reason")?,
            message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty()),
        )?;
    } else {
        f::fields(
            body,
            &[
                "model",
                "created_at",
                "message",
                "done",
                "done_reason",
                "total_duration",
                "load_duration",
                "prompt_eval_count",
                "prompt_eval_cached_count",
                "prompt_eval_duration",
                "eval_count",
                "eval_duration",
            ],
        )?;
        if body.get("done").and_then(Value::as_bool) != Some(true) {
            return Err(invalid());
        }
        let message = body.get("message").ok_or_else(invalid)?;
        validate_ollama_message(message)?;
        reason(
            family,
            f::text(body, "done_reason")?,
            message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty()),
        )?;
    }
    Ok(())
}
fn validate_ollama_message(message: &Value) -> Result<(), ProviderError> {
    f::fields(message, &["role", "content", "thinking", "tool_calls"])?;
    f::optional_role(message, "role", "assistant")?;
    f::optional_text(message, "content")?;
    f::optional_text(message, "thinking")?;
    calls(message.get("tool_calls"), Family::OllamaChat)
}
/// Ollama does not supply call IDs on all versions. Mint one namespace per
/// physical response; repeated call names or later responses cannot collide.
pub(crate) fn repair_tool_ids(response: &mut ChatResponseV1) -> Result<(), ProviderError> {
    if response.extensions.contains_key("ollama") {
        let supplied = response.extensions["ollama"]
            .pointer("/message/tool_calls")
            .and_then(Value::as_array);
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|_| invalid())?;
        let namespace: String = nonce.iter().map(|v| format!("{v:02x}")).collect();
        for (index, call) in response.output.tool_calls.iter_mut().enumerate() {
            if supplied
                .and_then(|v| v.get(index))
                .and_then(|v| v.get("id"))
                .is_none()
            {
                call.id = format!("ollama_{namespace}_{index}");
            }
        }
        if let Some(thinking) = response.extensions["ollama"]
            .pointer("/message/thinking")
            .and_then(Value::as_str)
        {
            response
                .extensions
                .insert("reasoning".into(), Value::String(thinking.into()));
        }
        if !response.output.tool_calls.is_empty() && response.finish_reason == Some(Finish::Stop) {
            response.finish_reason = Some(Finish::ToolCalls);
        }
    }
    crate::buffered_completion::unique_calls(&response.output.tool_calls)
}

pub(crate) fn decode(mut raw: crate::ByteStream, model: String, family: Family) -> ChatEventStream {
    Box::pin(async_stream::try_stream! {
        let mut sse=crate::native_stream::SseDecoder::new(Some(Family::CohereChat));
        let mut ndjson=crate::linesplit::LineSplitter::new(crate::MAX_STREAM_LINE_BYTES);
        let mut projection=Projection::new(family,model)?;
        let mut failure=None;
        while let Some(chunk)=raw.next().await {
            let chunk=match chunk {Ok(chunk)=>chunk,Err(error)=>Err(failure.take().unwrap_or(error))?};
            if failure.is_some(){continue;}
            if family==Family::CohereChat {
                sse.push(&chunk.data);
                loop {
                    let line=match sse.next_line(){Ok(Some((line,_)))=>line,Ok(None)=>break,Err(error)=>{failure=Some(error);break}};
                    if let Some(data)=line.strip_prefix(b"data:") {
                        match parse_project(data,&mut projection){Ok(events)=>for event in events{yield event;},Err(error)=>{failure=Some(error);break}}
                    }
                }
                if sse.over_budget(){failure.get_or_insert_with(invalid);}
            }else{
                // Bound allocations even if the upstream hands us a huge chunk.
                for part in chunk.data.chunks(8192) {
                    ndjson.push(part);
                    while let Some(line)=ndjson.next_line(){match parse_project(&line,&mut projection){Ok(events)=>for event in events{yield event;},Err(error)=>{failure=Some(error);break}}}
                    if ndjson.over_budget(){failure.get_or_insert_with(invalid);}
                    if failure.is_some(){break;}
                }
            }
        }
        if family==Family::OllamaChat && failure.is_none() && !ndjson.remainder().is_empty(){match parse_project(ndjson.remainder(),&mut projection){Ok(events)=>for event in events{yield event;},Err(error)=>failure=Some(error)}}
        if let Some(error)=failure{Err(error)?;}
        if !projection.open.is_empty(){Err(invalid())?;}
        yield Event::Finish{reason:projection.finish.ok_or_else(invalid)?};
    })
}
fn parse_project(data: &[u8], projection: &mut Projection) -> Result<Vec<Event>, ProviderError> {
    let value = sandhi_core::policy::strict_json(data).map_err(|_| invalid())?;
    projection.event(&value)
}
struct Call {
    id: String,
    name: String,
    args: String,
    closed: bool,
}
struct Projection {
    family: Family,
    model: String,
    started: bool,
    finish: Option<Finish>,
    open: BTreeMap<u32, Call>,
    ids: BTreeSet<String>,
    next: u32,
    namespace: String,
    phase: u8,
    content: Option<u32>,
    next_content: u32,
    retained_args: usize,
    wire_indexes: BTreeSet<u64>,
}
impl Projection {
    fn new(family: Family, model: String) -> Result<Self, ProviderError> {
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|_| invalid())?;
        Ok(Self {
            family,
            model,
            started: false,
            finish: None,
            open: BTreeMap::new(),
            ids: BTreeSet::new(),
            next: 0,
            namespace: nonce.iter().map(|v| format!("{v:02x}")).collect(),
            phase: 0,
            content: None,
            next_content: 0,
            retained_args: 0,
            wire_indexes: BTreeSet::new(),
        })
    }
    fn event(&mut self, v: &Value) -> Result<Vec<Event>, ProviderError> {
        if self.finish.is_some() {
            return Err(invalid());
        }
        if self.family == Family::OllamaChat {
            return self.ollama(v);
        }
        f::fields(v, &["type", "id", "index", "delta"])?;
        let kind = f::text(v, "type")?;
        let mut events = Vec::new();
        if kind == "message-start" {
            if self.started {
                return Err(invalid());
            }
            self.started = true;
            if let Some(delta) = v.get("delta") {
                f::fields(delta, &["message"])?;
                if let Some(message) = delta.get("message") {
                    f::fields(message, &["role", "content"])?;
                    f::optional_role(message, "role", "assistant")?;
                    if message
                        .get("content")
                        .is_some_and(|v| !v.is_null() && !v.as_array().is_some_and(Vec::is_empty))
                    {
                        return Err(invalid());
                    }
                }
            }
            events.push(Event::ResponseStart {
                id: Some(nonempty(v, "id")?),
                model: self.model.clone(),
            });
            return Ok(events);
        }
        if !self.started {
            return Err(invalid());
        }
        match kind {
            "content-start" => {
                let index = index(v)?;
                if self.content.is_some() || index != self.next_content || self.phase > 1 {
                    return Err(invalid());
                }
                self.next_content += 1;
                self.content = Some(index);
                self.phase = 1;
                let d = v.get("delta").ok_or_else(invalid)?;
                f::fields(d, &["message"])?;
                let m = d.get("message").ok_or_else(invalid)?;
                f::fields(m, &["content"])?;
                let c = m.get("content").ok_or_else(invalid)?;
                f::fields(c, &["type", "text"])?;
                if f::text(c, "type")? != "text" {
                    return Err(invalid());
                }
                let text = f::text(c, "text")?;
                if !text.is_empty() {
                    events.push(Event::TextDelta { delta: text.into() });
                }
            }
            "content-end" => {
                if self.content.take() != Some(index(v)?) {
                    return Err(invalid());
                }
                if let Some(delta) = v.get("delta") {
                    if !delta.is_null() && !delta.as_object().is_some_and(serde_json::Map::is_empty)
                    {
                        return Err(invalid());
                    }
                }
            }
            "tool-plan-delta" => {
                if self.phase > 1 || self.content.is_some() {
                    return Err(invalid());
                }
                self.phase = 1;
                let d = v.get("delta").ok_or_else(invalid)?;
                f::fields(d, &["message"])?;
                let m = d.get("message").ok_or_else(invalid)?;
                f::fields(m, &["tool_plan"])?;
                events.push(Event::TextDelta {
                    delta: f::text(m, "tool_plan")?.into(),
                });
            }
            "content-delta" => {
                if self.phase > 1 || self.content != Some(index(v)?) {
                    return Err(invalid());
                }
                self.phase = 1;
                let d = v.get("delta").ok_or_else(invalid)?;
                f::fields(d, &["message"])?;
                let m = d.get("message").ok_or_else(invalid)?;
                f::fields(m, &["content"])?;
                let c = m.get("content").ok_or_else(invalid)?;
                f::fields(c, &["text"])?;
                events.push(Event::TextDelta {
                    delta: f::text(c, "text")?.into(),
                });
            }
            "tool-call-start" => {
                if self.content.is_some() {
                    return Err(invalid());
                }
                self.phase = 2;
                let index = index(v)?;
                if self.open.len() >= 1024 || self.open.contains_key(&index) {
                    return Err(invalid());
                }
                let call = tool_delta(v)?;
                f::fields(call, &["id", "type", "function"])?;
                if call
                    .get("type")
                    .is_some_and(|v| v.as_str() != Some("function"))
                {
                    return Err(invalid());
                }
                let id = nonempty(call, "id")?;
                if !self.ids.insert(id.clone()) {
                    return Err(invalid());
                }
                let function = call.get("function").ok_or_else(invalid)?;
                f::fields(function, &["name", "arguments"])?;
                let name = nonempty(function, "name")?;
                let args = f::optional_text(function, "arguments")?.unwrap_or_default();
                if self.retained_args.saturating_add(args.len()) > crate::MAX_STREAM_LINE_BYTES {
                    return Err(invalid());
                }
                self.retained_args += args.len();
                events.push(Event::ToolCallStart {
                    index,
                    id: id.clone(),
                    name: name.clone(),
                });
                if !args.is_empty() {
                    events.push(Event::ToolCallArgumentsDelta {
                        index,
                        delta: args.clone(),
                    });
                }
                self.open.insert(
                    index,
                    Call {
                        id,
                        name,
                        args,
                        closed: false,
                    },
                );
            }
            "tool-call-delta" => {
                let index = index(v)?;
                let call = tool_delta(v)?;
                f::fields(call, &["id", "function"])?;
                let state = self
                    .open
                    .get_mut(&index)
                    .filter(|v| !v.closed)
                    .ok_or_else(invalid)?;
                if call
                    .get("id")
                    .is_some_and(|v| v.as_str() != Some(&state.id))
                {
                    return Err(invalid());
                }
                let function = call.get("function").ok_or_else(invalid)?;
                f::fields(function, &["name", "arguments"])?;
                if function
                    .get("name")
                    .is_some_and(|v| v.as_str() != Some(&state.name))
                {
                    return Err(invalid());
                }
                let args = f::text(function, "arguments")?;
                if self.retained_args.saturating_add(args.len()) > crate::MAX_STREAM_LINE_BYTES {
                    return Err(invalid());
                }
                self.retained_args += args.len();
                state.args.push_str(args);
                events.push(Event::ToolCallArgumentsDelta {
                    index,
                    delta: args.into(),
                });
            }
            "tool-call-end" => {
                let index = index(v)?;
                let call = self
                    .open
                    .get_mut(&index)
                    .filter(|v| !v.closed)
                    .ok_or_else(invalid)?;
                f::string_args(&Value::String(call.args.clone()))?;
                self.retained_args -= call.args.len();
                call.args.clear();
                call.closed = true;
                events.push(Event::ToolCallEnd { index });
            }
            "message-end" => {
                if self.content.is_some() || self.open.values().any(|v| !v.closed) {
                    return Err(invalid());
                }
                let delta = v.get("delta").ok_or_else(invalid)?;
                f::fields(delta, &["finish_reason", "usage"])?;
                self.finish = Some(reason(
                    self.family,
                    f::text(delta, "finish_reason")?,
                    !self.open.is_empty(),
                )?);
                self.open.clear();
            }
            _ => return Err(invalid()),
        }
        Ok(events)
    }
    fn ollama(&mut self, v: &Value) -> Result<Vec<Event>, ProviderError> {
        f::fields(
            v,
            &[
                "model",
                "created_at",
                "message",
                "done",
                "done_reason",
                "total_duration",
                "load_duration",
                "prompt_eval_count",
                "prompt_eval_cached_count",
                "prompt_eval_duration",
                "eval_count",
                "eval_duration",
            ],
        )?;
        let mut events = Vec::new();
        let model = nonempty(v, "model")?;
        if !self.started {
            self.started = true;
            self.model = model.clone();
            events.push(Event::ResponseStart {
                id: None,
                model: model.clone(),
            });
        } else if self.model != model {
            return Err(invalid());
        }
        let message = v.get("message").ok_or_else(invalid)?;
        validate_ollama_message(message)?;
        if let Some(thinking) = f::optional_text(message, "thinking")?.filter(|s| !s.is_empty()) {
            if self.phase > 0 {
                return Err(invalid());
            }
            events.push(Event::ReasoningDelta { delta: thinking });
        }
        if let Some(text) = f::optional_text(message, "content")?.filter(|s| !s.is_empty()) {
            if self.phase > 1 {
                return Err(invalid());
            }
            self.phase = 1;
            events.push(Event::TextDelta { delta: text });
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                if self.next >= 1024 {
                    return Err(invalid());
                }
                self.phase = 2;
                let index = self.next;
                self.next += 1;
                let id = match call.get("id") {
                    Some(_) => nonempty(call, "id")?,
                    None => format!("ollama_{}_{index}", self.namespace),
                };
                if !self.ids.insert(id.clone()) {
                    return Err(invalid());
                }
                let function = &call["function"];
                if let Some(index) = function.get("index") {
                    let index = index
                        .as_u64()
                        .filter(|index| *index < 1024)
                        .ok_or_else(invalid)?;
                    if !self.wire_indexes.insert(index) {
                        return Err(invalid());
                    }
                }
                let name = nonempty(function, "name")?;
                events.push(Event::ToolCallStart { index, id, name });
                events.push(Event::ToolCallArgumentsDelta {
                    index,
                    delta: function["arguments"].to_string(),
                });
                events.push(Event::ToolCallEnd { index });
            }
        }
        if v.get("done").and_then(Value::as_bool).ok_or_else(invalid)? {
            self.finish = Some(reason(
                self.family,
                f::text(v, "done_reason")?,
                self.next > 0,
            )?);
        }
        Ok(events)
    }
}
fn index(v: &Value) -> Result<u32, ProviderError> {
    v.get("index")
        .and_then(Value::as_u64)
        .filter(|v| *v < 1024)
        .map(|v| v as u32)
        .ok_or_else(invalid)
}
fn tool_delta(v: &Value) -> Result<&Value, ProviderError> {
    let d = v.get("delta").ok_or_else(invalid)?;
    f::fields(d, &["message"])?;
    let m = d.get("message").ok_or_else(invalid)?;
    f::fields(m, &["tool_calls"])?;
    m.get("tool_calls").ok_or_else(invalid)
}

/// Portable native chat has no server continuation resource that could resolve
/// an orphan result. Match every result to an outstanding caller-visible ID.
pub(crate) fn validate_history(request: &sandhi_core::ChatRequestV1) -> Result<(), ProviderError> {
    let mut outstanding = BTreeMap::new();
    for message in &request.messages {
        match message {
            sandhi_core::ChatMessageV1::Assistant { tool_calls, .. } => {
                for call in tool_calls {
                    let args = sandhi_core::policy::strict_json(call.arguments.as_bytes())
                        .map_err(|_| {
                            ProviderError::InvalidRequest(
                                "native tool arguments require a JSON object".into(),
                            )
                        })?;
                    if !args.is_object()
                        || call.id.is_empty()
                        || call.name.is_empty()
                        || outstanding
                            .insert(call.id.clone(), call.name.clone())
                            .is_some()
                    {
                        return Err(ProviderError::InvalidRequest(
                            "invalid or duplicate outstanding native tool call".into(),
                        ));
                    }
                }
            }
            sandhi_core::ChatMessageV1::Tool { tool_call_id, .. }
                if outstanding.remove(tool_call_id).is_none() =>
            {
                return Err(ProviderError::InvalidRequest(
                    "native tool result references an unknown or completed call".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}
