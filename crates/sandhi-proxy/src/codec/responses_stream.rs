//! Ordered, bounded presentation of portable output in the Responses protocol.
//! These IDs identify this delivery; they confer no provider continuation state.
use super::{object_tool_arguments, responses_status, responses_usage, unix_now};
use sandhi_core::{ChatStreamEventV1 as Event, UsageV2, MAX_NATIVE_STREAM_FRAME_BYTES};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

type Frames = Vec<(Option<&'static str>, Value)>;
const LIMIT: &str = "Responses stream output exceeds its retained presentation budget";

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Refusal,
    Reasoning,
}
enum Item {
    Prose {
        kind: Kind,
        text: String,
    },
    Call {
        call_id: String,
        name: String,
        arguments: String,
        done: bool,
    },
}

pub(super) struct Encoder {
    namespace: String,
    response_id: String,
    model: String,
    created: i64,
    started: bool,
    ended: bool,
    sequence: u64,
    items: Vec<Item>,
    open_prose: Option<usize>,
    calls: BTreeMap<u32, usize>,
    call_ids: BTreeSet<String>,
    retained: usize,
}
impl Encoder {
    pub(super) fn new() -> Self {
        let namespace = crate::next_request_id();
        Self {
            response_id: format!("resp_{namespace}"),
            namespace,
            model: String::new(),
            created: unix_now(),
            started: false,
            ended: false,
            sequence: 0,
            items: vec![],
            open_prose: None,
            calls: BTreeMap::new(),
            call_ids: BTreeSet::new(),
            retained: 0,
        }
    }

    fn retain(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.retained = self.retained.saturating_add(bytes);
        if self.retained > MAX_NATIVE_STREAM_FRAME_BYTES {
            return Err(LIMIT);
        }
        Ok(())
    }
    fn id(&self, index: usize) -> String {
        format!("item_{}_{index}", self.namespace)
    }
    fn part(kind: Kind, text: &str) -> Value {
        match kind {
            Kind::Text => json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]}),
            Kind::Refusal => json!({"type":"refusal","refusal":text}),
            Kind::Reasoning => json!({"type":"summary_text","text":text}),
        }
    }
    fn item(&self, index: usize, added: bool) -> Value {
        let id = self.id(index);
        match &self.items[index] {
            Item::Prose {
                kind: Kind::Reasoning,
                text,
            } => json!({
                "type":"reasoning","id":id,"summary":if added {vec![]} else {vec![Self::part(Kind::Reasoning,text)]}
            }),
            Item::Prose { kind, text } => json!({
                "type":"message","id":id,"role":"assistant","status":if added {"in_progress"} else {"completed"},
                "content":if added {vec![]} else {vec![Self::part(*kind,text)]}
            }),
            Item::Call {
                call_id,
                name,
                arguments,
                done,
            } => json!({
                "type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":arguments,
                "status":if *done {"completed"} else {"in_progress"}
            }),
        }
    }
    fn response(&self, status: &str, usage: Option<&UsageV2>) -> Value {
        json!({"id":self.response_id,"object":"response","created_at":self.created,
            "status":status,"model":self.model,"output":(0..self.items.len()).map(|i|self.item(i,false)).collect::<Vec<_>>(),
            "usage":usage.map(responses_usage),"error":Value::Null,"incomplete_details":Value::Null,
            "parallel_tool_calls":true,"tool_choice":"auto","tools":[],"temperature":Value::Null,"top_p":Value::Null})
    }
    fn close_prose(&mut self, frames: &mut Frames) {
        let Some(index) = self.open_prose.take() else {
            return;
        };
        let Item::Prose { kind, text } = &self.items[index] else {
            unreachable!()
        };
        let id = self.id(index);
        let (done, part_done) = match kind {
            Kind::Text => ("response.output_text.done", "response.content_part.done"),
            Kind::Refusal => ("response.refusal.done", "response.content_part.done"),
            Kind::Reasoning => (
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
            ),
        };
        let mut value = json!({"type":done,"item_id":id,"output_index":index});
        if *kind == Kind::Reasoning {
            value["summary_index"] = json!(0);
        } else {
            value["content_index"] = json!(0);
        }
        if *kind == Kind::Refusal {
            value["refusal"] = json!(text);
        } else {
            value["text"] = json!(text);
        }
        if *kind == Kind::Text {
            value["logprobs"] = json!([]);
        }
        frames.push((Some(done), value));
        let mut value = json!({"type":part_done,"item_id":id,"output_index":index,"part":Self::part(*kind,text)});
        value[if *kind == Kind::Reasoning {
            "summary_index"
        } else {
            "content_index"
        }] = json!(0);
        frames.push((Some(part_done), value));
        frames.push((Some("response.output_item.done"),json!({"type":"response.output_item.done","output_index":index,"item":self.item(index,false)})));
    }
    fn prose(&mut self, kind: Kind, delta: &str, frames: &mut Frames) -> Result<(), &'static str> {
        self.retain(delta.len())?;
        let reuse = self
            .open_prose
            .filter(|i| matches!(&self.items[*i], Item::Prose{kind:k,..} if *k==kind));
        let index = if let Some(index) = reuse {
            index
        } else {
            if self.items.len() >= 1024 {
                return Err(LIMIT);
            }
            self.close_prose(frames);
            let index = self.items.len();
            self.items.push(Item::Prose {
                kind,
                text: String::new(),
            });
            self.open_prose = Some(index);
            frames.push((Some("response.output_item.added"),json!({"type":"response.output_item.added","output_index":index,"item":self.item(index,true)})));
            let added = if kind == Kind::Reasoning {
                "response.reasoning_summary_part.added"
            } else {
                "response.content_part.added"
            };
            let mut value = json!({"type":added,"item_id":self.id(index),"output_index":index,"part":Self::part(kind,"")});
            value[if kind == Kind::Reasoning {
                "summary_index"
            } else {
                "content_index"
            }] = json!(0);
            frames.push((Some(added), value));
            index
        };
        let Item::Prose { text, .. } = &mut self.items[index] else {
            unreachable!()
        };
        text.push_str(delta);
        let event = match kind {
            Kind::Text => "response.output_text.delta",
            Kind::Refusal => "response.refusal.delta",
            Kind::Reasoning => "response.reasoning_summary_text.delta",
        };
        let mut value =
            json!({"type":event,"item_id":self.id(index),"output_index":index,"delta":delta});
        value[if kind == Kind::Reasoning {
            "summary_index"
        } else {
            "content_index"
        }] = json!(0);
        if kind == Kind::Text {
            value["logprobs"] = json!([]);
        }
        frames.push((Some(event), value));
        Ok(())
    }

    pub(super) fn encode(
        &mut self,
        event: &Event,
        usage: Option<&UsageV2>,
    ) -> Result<Frames, &'static str> {
        if self.ended {
            return Err("Responses stream event follows its terminal");
        }
        let mut frames = vec![];
        match event {
            Event::ResponseStart { id, model } => {
                if self.started {
                    return Err("duplicate Responses stream start");
                }
                self.started = true;
                if let Some(id) = id {
                    self.response_id = id.clone();
                }
                self.model.clone_from(model);
                frames.push((
                    Some("response.created"),
                    json!({"type":"response.created","response":self.response("in_progress",None)}),
                ));
            }
            Event::TextDelta { delta } => self.prose(Kind::Text, delta, &mut frames)?,
            Event::RefusalDelta { delta } => self.prose(Kind::Refusal, delta, &mut frames)?,
            Event::ReasoningDelta { delta } => self.prose(Kind::Reasoning, delta, &mut frames)?,
            Event::ToolCallStart { index, id, name } => {
                if self.calls.contains_key(index)
                    || self.call_ids.contains(id)
                    || id.is_empty()
                    || name.is_empty()
                    || id.len() > 256
                    || name.len() > 256
                {
                    return Err("invalid or duplicate tool-call identity in Responses output");
                }
                if self.items.len() >= 1024 {
                    return Err(LIMIT);
                }
                self.retain(id.len().saturating_add(name.len()))?;
                self.close_prose(&mut frames);
                let slot = self.items.len();
                self.items.push(Item::Call {
                    call_id: id.clone(),
                    name: name.clone(),
                    arguments: String::new(),
                    done: false,
                });
                self.calls.insert(*index, slot);
                self.call_ids.insert(id.clone());
                frames.push((Some("response.output_item.added"),json!({"type":"response.output_item.added","output_index":slot,"item":self.item(slot,true)})));
            }
            Event::ToolCallArgumentsDelta { index, delta } => {
                let slot = *self
                    .calls
                    .get(index)
                    .ok_or("Responses tool arguments have no matching start")?;
                self.retain(delta.len())?;
                let Item::Call {
                    arguments, done, ..
                } = &mut self.items[slot]
                else {
                    unreachable!()
                };
                if *done {
                    return Err("Responses tool arguments follow tool completion");
                }
                arguments.push_str(delta);
                frames.push((Some("response.function_call_arguments.delta"),json!({"type":"response.function_call_arguments.delta","output_index":slot,"item_id":self.id(slot),"delta":delta})));
            }
            Event::ToolCallEnd { index } => {
                let slot = *self
                    .calls
                    .get(index)
                    .ok_or("Responses tool end has no matching start")?;
                let Item::Call {
                    arguments, done, ..
                } = &mut self.items[slot]
                else {
                    unreachable!()
                };
                if *done {
                    return Err("duplicate Responses tool end");
                }
                object_tool_arguments(arguments)?;
                *done = true;
                let arguments = arguments.clone();
                frames.push((Some("response.function_call_arguments.done"),json!({"type":"response.function_call_arguments.done","output_index":slot,"item_id":self.id(slot),"arguments":arguments})));
                frames.push((Some("response.output_item.done"),json!({"type":"response.output_item.done","output_index":slot,"item":self.item(slot,false)})));
            }
            Event::Finish { reason } => {
                if self
                    .items
                    .iter()
                    .any(|i| matches!(i, Item::Call { done: false, .. }))
                {
                    return Err("Responses output ended with an unfinished tool call");
                }
                self.close_prose(&mut frames);
                let status = responses_status(Some(*reason));
                let terminal = if status == "incomplete" {
                    "response.incomplete"
                } else {
                    "response.completed"
                };
                let mut response =
                    self.response(status, Some(usage.unwrap_or(&UsageV2::default())));
                if status == "incomplete" {
                    response["incomplete_details"] = json!({"reason":if *reason==sandhi_core::FinishReasonV1::Length {"max_output_tokens"} else {"content_filter"}});
                }
                frames.push((Some(terminal), json!({"type":terminal,"response":response})));
                self.ended = true;
            }
            Event::Error { error } => {
                frames.push((Some("error"),json!({"type":"error","code":error.code,"message":error.message,"param":Value::Null})));
                self.ended = true;
            }
            Event::Usage { .. } | Event::NativeFrame { .. } => {}
        }
        for (_, value) in &mut frames {
            value["sequence_number"] = json!(self.sequence);
            self.sequence += 1;
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::FinishReasonV1;

    #[test]
    fn terminal_fold_keeps_refusal_reasoning_and_text_after_tools_in_event_order() {
        let mut encoder = Encoder::new();
        let mut frames = vec![];
        let events = [
            Event::ReasoningDelta {
                delta: "consider".into(),
            },
            Event::TextDelta {
                delta: "before".into(),
            },
            Event::ToolCallStart {
                index: 7,
                id: "call_a".into(),
                name: "lookup".into(),
            },
            Event::ToolCallArgumentsDelta {
                index: 7,
                delta: "{}".into(),
            },
            Event::ToolCallEnd { index: 7 },
            Event::TextDelta {
                delta: "after".into(),
            },
            Event::RefusalDelta {
                delta: "declined".into(),
            },
            Event::Finish {
                reason: FinishReasonV1::ContentFilter,
            },
        ];
        for event in &events {
            frames.extend(encoder.encode(event, None).unwrap());
        }
        let output = frames.last().unwrap().1["response"]["output"]
            .as_array()
            .unwrap();
        assert_eq!(output.len(), 5);
        assert_eq!(output[0]["summary"][0]["text"], "consider");
        assert_eq!(output[1]["content"][0]["text"], "before");
        assert_eq!(output[2]["call_id"], "call_a");
        assert_eq!(output[3]["content"][0]["text"], "after");
        assert_eq!(
            output[4]["content"][0],
            json!({"type":"refusal","refusal":"declined"})
        );
        assert_eq!(
            frames.last().unwrap().1["response"]["incomplete_details"]["reason"],
            "content_filter"
        );
        for (sequence, (_, frame)) in frames.iter().enumerate() {
            assert_eq!(frame["sequence_number"], sequence);
        }
        for (index, item) in output.iter().enumerate() {
            let lifecycle: Vec<_> = frames
                .iter()
                .filter(|(_, frame)| frame["output_index"] == index && frame["item"].is_object())
                .map(|(_, frame)| frame)
                .collect();
            assert_eq!(lifecycle.len(), 2);
            assert_eq!(lifecycle[0]["type"], "response.output_item.added");
            assert_eq!(lifecycle[1]["type"], "response.output_item.done");
            assert_eq!(&lifecycle[1]["item"], item);
            assert_eq!(lifecycle[0]["item"]["id"], item["id"]);
        }
    }

    #[test]
    fn tool_lifecycle_arguments_and_retained_output_are_bounded() {
        let start = Event::ToolCallStart {
            index: 0,
            id: "call".into(),
            name: "f".into(),
        };
        for invalid in ["[]", "null", "", r#"{"x":1,"x":2}"#] {
            let mut encoder = Encoder::new();
            encoder.encode(&start, None).unwrap();
            encoder
                .encode(
                    &Event::ToolCallArgumentsDelta {
                        index: 0,
                        delta: invalid.into(),
                    },
                    None,
                )
                .unwrap();
            assert!(encoder
                .encode(&Event::ToolCallEnd { index: 0 }, None)
                .is_err());
        }
        let mut encoder = Encoder::new();
        assert!(encoder
            .encode(&Event::ToolCallEnd { index: 0 }, None)
            .is_err());
        encoder.encode(&start, None).unwrap();
        assert!(encoder.encode(&start, None).is_err());
        assert!(encoder
            .encode(
                &Event::Finish {
                    reason: FinishReasonV1::Stop
                },
                None
            )
            .is_err());
        let mut encoder = Encoder::new();
        encoder.retained = MAX_NATIVE_STREAM_FRAME_BYTES;
        assert!(encoder
            .encode(&Event::ReasoningDelta { delta: "x".into() }, None)
            .is_err());
        // Empty alternating deltas cannot bypass the item-cardinality bound.
        let mut encoder = Encoder::new();
        for i in 0..1024 {
            let event = if i % 2 == 0 {
                Event::TextDelta {
                    delta: String::new(),
                }
            } else {
                Event::ReasoningDelta {
                    delta: String::new(),
                }
            };
            encoder.encode(&event, None).unwrap();
        }
        assert!(encoder
            .encode(
                &Event::RefusalDelta {
                    delta: String::new()
                },
                None
            )
            .is_err());
    }
}
