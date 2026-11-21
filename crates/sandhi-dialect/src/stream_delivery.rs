use crate::{IngressDialect, StreamEventEncoder};
use sandhi_core::{
    ChatStreamEventV1, FinishReasonV1, UsageCompleteness, UsageV2, MAX_NATIVE_STREAM_FRAME_BYTES,
};
use serde_json::Value;

pub type Frames = Vec<(Option<&'static str>, Value)>;

/// A reported source failure already has a protocol frame. The transport must
/// avoid adding another frame when the source observer reports the same failure.
pub fn is_native_stream_error(value: &Value) -> bool {
    value.get("error").is_some_and(|error| !error.is_null())
        || matches!(
            value.get("type").and_then(Value::as_str),
            Some("error" | "response.failed")
        )
}

/// Render a translated stream in protocol order, including final usage and a
/// deferred finish. Call `finish` only after successful source EOF.
pub struct TranslatedStream {
    dialect: IngressDialect,
    encoder: StreamEventEncoder,
    finish: Option<FinishReasonV1>,
    final_usage: Option<UsageV2>,
    projection_bytes: Option<usize>,
}

impl TranslatedStream {
    pub fn new(dialect: IngressDialect, bound_projection: bool) -> Self {
        Self {
            dialect,
            encoder: StreamEventEncoder::new(dialect),
            finish: None,
            final_usage: None,
            projection_bytes: bound_projection.then_some(0),
        }
    }

    pub fn push(&mut self, event: &ChatStreamEventV1) -> Result<Frames, &'static str> {
        use ChatStreamEventV1 as Event;
        if let Some(total) = &mut self.projection_bytes {
            // Some dialects retain text or tool arguments across frames. Bound
            // the whole projection separately from transport backpressure.
            let payload = match event {
                Event::TextDelta { delta }
                | Event::ReasoningDelta { delta }
                | Event::RefusalDelta { delta }
                | Event::ToolCallArgumentsDelta { delta, .. } => delta.len(),
                Event::ToolCallStart { id, name, .. } => id.len().saturating_add(name.len()),
                Event::ResponseStart { id, model } => id
                    .as_ref()
                    .map_or(0, String::len)
                    .saturating_add(model.len()),
                _ => 0,
            };
            *total = total.saturating_add(payload).saturating_add(128);
            if *total > MAX_NATIVE_STREAM_FRAME_BYTES {
                return Err("translated stream projection limit exceeded");
            }
        }
        match event {
            ChatStreamEventV1::Finish { reason } => {
                self.finish = Some(*reason);
                return Ok(Vec::new());
            }
            ChatStreamEventV1::Usage { usage } => {
                if usage.completeness != UsageCompleteness::Final {
                    return Ok(Vec::new());
                }
                self.final_usage = Some(usage.clone());
                if self.dialect == IngressDialect::OpenAi {
                    return Ok(Vec::new());
                }
            }
            _ => {}
        }
        self.encoder.encode(event, self.final_usage.as_ref())
    }

    pub fn finish(mut self) -> Result<Frames, &'static str> {
        let mut frames = Vec::new();
        if let Some(reason) = self.finish {
            frames.extend(self.encoder.encode(
                &ChatStreamEventV1::Finish { reason },
                self.final_usage.as_ref(),
            )?);
        }
        if self.dialect == IngressDialect::OpenAi {
            if let Some(usage) = self.final_usage {
                frames.extend(
                    self.encoder
                        .encode(&ChatStreamEventV1::Usage { usage }, None)?,
                );
            }
        }
        Ok(frames)
    }
}
