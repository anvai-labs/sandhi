//! Response-owned protocol usage observation from complete, bounded SSE events.
//!
//! Create one observer per upstream HTTP response and keep it with the existing
//! request/attempt owner. SSE `id` never becomes Sandhi attribution. This observer
//! performs no I/O, retries, persistence or settlement and does not enable tracked
//! streaming. Legacy byte forwarding and metering are unchanged.

use crate::{linesplit::LineSplitter, ParsedUsage, MAX_STREAM_LINE_BYTES};
use sandhi_core::usage::{
    qualify_openai_stream_usage_for_profile, AnthropicUsageProfile, OpenAiStreamUsageProfile,
    StreamUsageError,
};
use sandhi_core::{EndpointFamilyV1, UsageCompleteness, UsageV2};
use serde_json::Value;

mod anthropic;
#[cfg(test)]
mod anthropic_tests;
#[cfg(test)]
mod chat_profile_tests;
mod gemini;
#[cfg(test)]
mod gemini_tests;
mod native_chat;
mod responses;
#[cfg(test)]
mod responses_tests;
mod text_completion;

// Bound the extra copy when a caller supplies an arbitrarily large HTTP chunk.
const INPUT_SLICE_BYTES: usize = 8192;

/// Static failure facts only: no upstream content, identifiers or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamObservationError {
    LineTooLong,
    EventTooLong,
    InvalidUtf8,
    InvalidJson,
    UnexpectedEventType,
    UnexpectedBom,
    InvalidUsage(StreamUsageError),
    ConflictingUsage,
    TruncatedEvent,
    MissingTerminator,
    DataAfterTerminator,
    Closed,
    UnsupportedEndpoint,
    InvalidFamilyUsage(sandhi_core::usage::BufferedUsageError),
    InvalidEnvelope,
    IdentityMismatch,
    ProviderEvent,
}

/// Response-scoped measurement, independent of delivery success or settlement.
///
/// Chat totals follow the declared placement profile. Responses terminal objects
/// and cumulative Messages/Gemini updates use their family qualifiers. Messages
/// progress becomes final only at a qualified `message_stop`. Gemini additionally
/// requires clean HTTP EOF after candidate completion and a complete final usage
/// frame; abort never promotes Gemini progress. Canonical evidence preserves
/// partial completeness; legacy [`usage`](Self::usage) exposes final measurements
/// only. Malformed/conflicting later events retain prior observations and set a
/// sticky [`error`](Self::error). Complete post-terminal data events fail explicitly.
///
/// SSE lines and events each have the existing 8 MiB ceiling, including comments
/// and ignored fields. Incoming chunks are inspected in bounded slices. CR/LF/CRLF,
/// one initial BOM, multiline data and ordinary message events are supported. EOF
/// never dispatches a partial event. This is not an EventSource/reconnection client.
pub struct OpenAiStreamUsageObserver {
    text_completion: Option<text_completion::TextCompletion>,
    gateway_peer: bool,
    gateway_failure: Option<crate::GatewayFailure>,
    splitter: LineSplitter,
    limit: usize,
    first_line: bool,
    event_bytes: usize,
    data: Vec<u8>,
    has_data: bool,
    message_event: bool,
    event_name: Option<String>,
    endpoint: EndpointFamilyV1,
    openai_usage_profile: OpenAiStreamUsageProfile,
    anthropic_usage_profile: AnthropicUsageProfile,
    anthropic: anthropic::AnthropicUsage,
    responses: responses::ResponsesUsage,
    gemini: gemini::GeminiUsage,
    native_chat: native_chat::NativeChatUsage,
    usage: Option<ParsedUsage>,
    canonical_usage: Option<UsageV2>,
    cache_read_observation: Option<sandhi_core::CacheReadObservation>,
    error: Option<StreamObservationError>,
    done: bool,
    closed: bool,
    #[cfg(test)]
    peak_buffered: usize,
}

/// Complete-event observer shared across supported endpoint protocols. The old
/// name and default constructor retain OpenAI Chat behavior.
pub type ProtocolStreamUsageObserver = OpenAiStreamUsageObserver;

impl Default for OpenAiStreamUsageObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiStreamUsageObserver {
    pub fn new() -> Self {
        Self::with_limit(MAX_STREAM_LINE_BYTES)
    }

    /// Strict source observation for the authorized native Text Completion path.
    pub fn for_text_completion() -> Self {
        let mut observer = Self::new();
        observer.text_completion = Some(text_completion::TextCompletion::default());
        observer
    }

    pub fn for_endpoint(endpoint: EndpointFamilyV1) -> Result<Self, StreamObservationError> {
        Self::for_endpoint_with_openai_usage_profile(endpoint, OpenAiStreamUsageProfile::Standard)
    }

    pub fn for_endpoint_with_openai_usage_profile(
        endpoint: EndpointFamilyV1,
        profile: OpenAiStreamUsageProfile,
    ) -> Result<Self, StreamObservationError> {
        Self::for_endpoint_with_usage_profiles(endpoint, profile, AnthropicUsageProfile::default())
    }

    /// Endpoint conventions are explicit and family-scoped. This constructor
    /// observes evidence only; it does not enable tracked routing for a family.
    pub fn for_endpoint_with_usage_profiles(
        endpoint: EndpointFamilyV1,
        openai_profile: OpenAiStreamUsageProfile,
        anthropic_profile: AnthropicUsageProfile,
    ) -> Result<Self, StreamObservationError> {
        if (openai_profile != OpenAiStreamUsageProfile::Standard
            && endpoint != EndpointFamilyV1::OpenaiChatCompletions)
            || (anthropic_profile != AnthropicUsageProfile::SeparateCacheWrites
                && endpoint != EndpointFamilyV1::AnthropicMessages)
        {
            return Err(StreamObservationError::UnsupportedEndpoint);
        }
        if !matches!(
            endpoint,
            EndpointFamilyV1::OpenaiChatCompletions
                | EndpointFamilyV1::OpenaiResponses
                | EndpointFamilyV1::AnthropicMessages
                | EndpointFamilyV1::GeminiGenerateContent
                | EndpointFamilyV1::CohereChat
                | EndpointFamilyV1::OllamaChat
        ) {
            return Err(StreamObservationError::UnsupportedEndpoint);
        }
        let mut observer = Self::new();
        if endpoint == EndpointFamilyV1::OllamaChat {
            observer.splitter = LineSplitter::new(MAX_STREAM_LINE_BYTES);
        }
        observer.endpoint = endpoint;
        observer.openai_usage_profile = openai_profile;
        observer.anthropic_usage_profile = anthropic_profile;
        Ok(observer)
    }

    fn with_limit(limit: usize) -> Self {
        Self {
            text_completion: None,
            gateway_peer: false,
            gateway_failure: None,
            splitter: LineSplitter::new_sse(limit),
            limit,
            first_line: true,
            event_bytes: 0,
            data: Vec::new(),
            has_data: false,
            message_event: true,
            event_name: None,
            endpoint: EndpointFamilyV1::OpenaiChatCompletions,
            openai_usage_profile: OpenAiStreamUsageProfile::Standard,
            anthropic_usage_profile: AnthropicUsageProfile::SeparateCacheWrites,
            anthropic: Default::default(),
            responses: Default::default(),
            gemini: Default::default(),
            native_chat: Default::default(),
            usage: None,
            canonical_usage: None,
            cache_read_observation: None,
            error: None,
            done: false,
            closed: false,
            #[cfg(test)]
            peak_buffered: 0,
        }
    }

    pub(crate) fn with_gateway_peer(mut self) -> Self {
        self.gateway_peer = true;
        self
    }
    pub(crate) fn gateway_peer(&self) -> bool {
        self.gateway_peer
    }
    pub(crate) fn gateway_failure(&self) -> Option<crate::GatewayFailure> {
        self.gateway_failure.clone()
    }
    fn observe_gateway_failure(&mut self, event: &Value) {
        if self.gateway_peer && self.gateway_failure.is_none() {
            if let Some(error) = event
                .get("error")
                .or_else(|| event.pointer("/response/error"))
                .filter(|error| !error.is_null())
            {
                self.gateway_failure =
                    Some(crate::peer::failure_from_value(Some(error), None, None));
            }
        }
    }

    pub(crate) fn cache_read_observation(&self) -> Option<sandhi_core::CacheReadObservation> {
        self.cache_read_observation
    }

    fn observe_cache(&mut self, event: &Value) {
        use sandhi_core::{CacheReadFamily as C, EndpointFamilyV1 as F};
        let family = match self.endpoint {
            F::OpenaiChatCompletions => C::OpenAi,
            F::OpenaiResponses => C::OpenAiResponses,
            F::AnthropicMessages => C::Anthropic,
            F::GeminiGenerateContent => C::Gemini,
            F::CohereChat => C::Cohere,
            F::OllamaChat => C::Ollama,
            _ => return,
        };
        sandhi_core::merge_cache_read_observation(
            &mut self.cache_read_observation,
            sandhi_core::observe_cache_read_stream(family, event),
        );
        self.cache_read_observation.get_or_insert_with(|| {
            sandhi_core::CacheReadObservation::origin(sandhi_core::CacheReadStatus::Absent)
        });
    }

    pub fn usage(&self) -> Option<ParsedUsage> {
        self.usage
    }

    /// Canonical evidence includes field presence and progress completeness.
    /// Legacy `usage()` remains a final-measurement projection only.
    pub fn canonical_usage(&self) -> Option<UsageV2> {
        self.canonical_usage.clone().map(|mut usage| {
            if self.error.is_some() {
                usage.completeness = UsageCompleteness::Partial;
            }
            usage
        })
    }

    pub fn provider_failed(&self) -> bool {
        self.gateway_failure.is_some()
            || self.responses.provider_failed
            || self.anthropic.provider_failed
            || self.gemini.provider_failed
            || self.native_chat.provider_failed
    }

    pub fn error(&self) -> Option<StreamObservationError> {
        self.error
    }

    /// A complete protocol terminal was observed; this does not imply usage or success.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Observe response bytes without modifying them. Errors stop qualification
    /// permanently; the first accepted measurement remains available for recovery.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), StreamObservationError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.closed {
            return Err(StreamObservationError::Closed);
        }
        for part in chunk.chunks(INPUT_SLICE_BYTES) {
            self.splitter.push(part);
            #[cfg(test)]
            {
                self.peak_buffered = self.peak_buffered.max(self.splitter.buffered_len());
            }
            while let Some(line) = self.splitter.next_line() {
                if let Err(error) = self.line(&line) {
                    return self.fail(error);
                }
            }
            if self.splitter.over_budget() {
                return self.fail(StreamObservationError::LineTooLong);
            }
        }
        Ok(())
    }

    /// Record EOF without manufacturing a blank line or measurement. Idempotent.
    /// Cancellation/transport failure remains the caller's separate outcome fact.
    pub fn finish(&mut self) -> Result<(), StreamObservationError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.closed && !self.done && self.endpoint == EndpointFamilyV1::GeminiGenerateContent {
            return self.fail(StreamObservationError::Closed);
        }
        self.closed = true;
        // NDJSON permits a complete final object without a trailing newline.
        if self.endpoint == EndpointFamilyV1::OllamaChat && !self.splitter.remainder().is_empty() {
            let remainder = self.splitter.remainder().to_vec();
            self.splitter.reset();
            if let Err(error) = self.native_event(&remainder) {
                return self.fail(error);
            }
        }
        if matches!(
            self.endpoint,
            EndpointFamilyV1::CohereChat | EndpointFamilyV1::OllamaChat
        ) && self.native_chat.done
        {
            let mut usage = self
                .native_chat
                .usage
                .clone()
                .expect("validated native terminal");
            usage.completeness = UsageCompleteness::Final;
            self.usage = Some((&usage).into());
            self.canonical_usage = Some(usage);
        }
        if !self.splitter.remainder().is_empty() || self.event_bytes != 0 {
            return self.fail(StreamObservationError::TruncatedEvent);
        }
        if self.endpoint == EndpointFamilyV1::GeminiGenerateContent && !self.done {
            let usage = match self.gemini.finish() {
                Ok(usage) => usage,
                Err(error) => return self.fail(error),
            };
            self.usage = Some((&usage).into());
            self.canonical_usage = Some(usage);
            self.done = true;
        }
        if !self.done {
            return self.fail(StreamObservationError::MissingTerminator);
        }
        Ok(())
    }

    /// Cancellation is not HTTP EOF and does not prove a missing terminator.
    /// Already-observed unfinished framing still invalidates final qualification.
    pub(crate) fn close_on_abort(&mut self) {
        self.closed = true;
        let pending_line = self.splitter.remainder();
        // An interrupted comment cannot become a data event. Large keepalives
        // can span HTTP chunks; their suffix must not erase a prior measurement.
        let unfinished_data =
            self.has_data || (!pending_line.is_empty() && !pending_line.starts_with(b":"));
        if self.error.is_none() && unfinished_data {
            let _ = self.fail(StreamObservationError::TruncatedEvent);
        }
    }

    fn fail(&mut self, error: StreamObservationError) -> Result<(), StreamObservationError> {
        self.error = Some(error);
        self.splitter.reset();
        self.data.clear();
        Err(error)
    }

    fn line(&mut self, line: &[u8]) -> Result<(), StreamObservationError> {
        if line.len() > self.limit {
            return Err(StreamObservationError::LineTooLong);
        }
        if self.endpoint == EndpointFamilyV1::OllamaChat {
            return self.native_event(line);
        }
        self.event_bytes += line.len();
        if self.event_bytes > self.limit {
            return Err(StreamObservationError::EventTooLong);
        }
        let mut content = &line[..line.len() - 1]; // Splitter includes exactly one terminator.
        if self.first_line {
            content = content.strip_prefix(b"\xef\xbb\xbf").unwrap_or(content);
            self.first_line = false;
        }
        if content.starts_with(b"\xef\xbb\xbf") {
            return Err(StreamObservationError::UnexpectedBom);
        }
        let content =
            std::str::from_utf8(content).map_err(|_| StreamObservationError::InvalidUtf8)?;
        if content.is_empty() {
            self.event()?;
            self.event_bytes = 0;
            self.data.clear();
            self.has_data = false;
            self.message_event = true;
            self.event_name = None;
        } else if !content.starts_with(':') {
            let (field, value) = content.split_once(':').unwrap_or((content, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => {
                    if self.has_data {
                        self.data.push(b'\n');
                    }
                    self.data.extend_from_slice(value.as_bytes());
                    self.has_data = true;
                }
                "event" => {
                    self.message_event = value.is_empty() || value == "message";
                    self.event_name = (value.len() <= 128).then(|| value.to_owned());
                }
                _ => {} // id/retry are not request correlation or replay permission.
            }
        }
        Ok(())
    }

    fn native_event(&mut self, data: &[u8]) -> Result<(), StreamObservationError> {
        let event = sandhi_core::policy::strict_json(data)
            .map_err(|_| StreamObservationError::InvalidJson)?;
        if self.endpoint == EndpointFamilyV1::CohereChat
            && self.event_name.as_deref().is_some_and(|name| {
                name != "message" && Some(name) != event.get("type").and_then(Value::as_str)
            })
        {
            return Err(StreamObservationError::UnexpectedEventType);
        }
        self.observe_cache(&event);
        if let Some(update) = self.native_chat.event(self.endpoint, &event)? {
            self.canonical_usage = Some(update);
        }
        self.done = self.native_chat.done;
        Ok(())
    }

    fn event(&mut self) -> Result<(), StreamObservationError> {
        if !self.has_data {
            return Ok(());
        }
        if self.endpoint == EndpointFamilyV1::CohereChat {
            let data = self.data.clone();
            return self.native_event(&data);
        }
        if self.endpoint == EndpointFamilyV1::GeminiGenerateContent {
            let event = sandhi_core::policy::strict_json(&self.data)
                .map_err(|_| StreamObservationError::InvalidJson)?;
            self.observe_gateway_failure(&event);
            self.observe_cache(&event);
            if let Some(update) = self.gemini.event(&event, self.message_event)? {
                self.canonical_usage = Some(update);
            }
            return Ok(());
        }
        if self.endpoint == EndpointFamilyV1::AnthropicMessages {
            if self.done {
                return Err(StreamObservationError::DataAfterTerminator);
            }
            let event = sandhi_core::policy::strict_json(&self.data)
                .map_err(|_| StreamObservationError::InvalidJson)?;
            self.observe_gateway_failure(&event);
            self.observe_cache(&event);
            let update = self.anthropic.event(
                &event,
                self.message_event,
                self.event_name.as_deref(),
                self.anthropic_usage_profile,
            );
            self.done = self.anthropic.done;
            if let Some(update) = update? {
                if update.completeness == UsageCompleteness::Final {
                    self.usage = Some((&update).into());
                }
                self.canonical_usage = Some(update);
            }
            return Ok(());
        }
        if self.endpoint == EndpointFamilyV1::OpenaiResponses {
            if self.data == b"[DONE]" {
                if !self.message_event {
                    return Err(StreamObservationError::UnexpectedEventType);
                }
                return self.responses.tail_done(self.done);
            }
            if self.done {
                return Err(StreamObservationError::DataAfterTerminator);
            }
            let event = sandhi_core::policy::strict_json(&self.data)
                .map_err(|_| StreamObservationError::InvalidJson)?;
            self.observe_gateway_failure(&event);
            self.observe_cache(&event);
            let update =
                self.responses
                    .event(&event, self.message_event, self.event_name.as_deref());
            self.done = self.responses.done;
            let update = update?;
            if let Some(update) = update {
                if update.completeness == UsageCompleteness::Final {
                    self.usage = Some((&update).into());
                }
                self.canonical_usage = Some(update);
            }
            return Ok(());
        }
        if self.done {
            return Err(StreamObservationError::DataAfterTerminator);
        }
        if !self.message_event {
            return Err(StreamObservationError::UnexpectedEventType);
        }
        if self.data == b"[DONE]" {
            if let Some(text) = &self.text_completion {
                text.finish()?;
            }
            self.done = true;
            return Ok(());
        }
        if self.usage.is_some()
            && matches!(
                self.openai_usage_profile,
                OpenAiStreamUsageProfile::ZaiFinalChoice
                    | OpenAiStreamUsageProfile::DeepSeekFinalChoice
            )
        {
            // These profiles declare one final choice. More data before [DONE]
            // invalidates qualification without erasing the first source totals.
            return Err(StreamObservationError::InvalidUsage(
                StreamUsageError::NonFinalUsage,
            ));
        }
        let event = sandhi_core::policy::strict_json(&self.data)
            .map_err(|_| StreamObservationError::InvalidJson)?;
        self.observe_gateway_failure(&event);
        self.observe_cache(&event);
        if let Some(text) = &mut self.text_completion {
            if let Some(usage) = text.event(&event)? {
                self.usage = Some((&usage).into());
                self.canonical_usage = Some(usage);
            }
            return Ok(());
        }
        if let Some(next) =
            qualify_openai_stream_usage_for_profile(&event, self.openai_usage_profile)
                .map_err(StreamObservationError::InvalidUsage)?
        {
            match self.usage {
                None => {
                    let mut canonical: UsageV2 = next.into();
                    canonical.reasoning_tokens = event
                        .pointer("/usage/completion_tokens_details/reasoning_tokens")
                        .and_then(Value::as_u64);
                    self.canonical_usage = Some(canonical);
                    self.usage = Some(next);
                }
                Some(first) if first == next => {
                    // A repeated identical total can add explicit optional zero
                    // presence; absence in the first frame must not erase it.
                    if let Some(canonical) = self.canonical_usage.as_mut() {
                        if canonical.reasoning_tokens.is_none() {
                            canonical.reasoning_tokens = event
                                .pointer("/usage/completion_tokens_details/reasoning_tokens")
                                .and_then(Value::as_u64);
                        }
                    }
                }
                Some(_) => return Err(StreamObservationError::ConflictingUsage),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(tokens: u64) -> String {
        format!(
            "data: {}\n\n",
            json!({"object":"chat.completion.chunk","choices":[],
            "usage":{"prompt_tokens":tokens,"completion_tokens":4}})
        )
    }

    #[test]
    fn stream_observer_multiline_framing_is_invariant_at_every_byte_boundary() {
        for newline in ["\n", "\r\n", "\r"] {
            let wire = concat!(
                "\u{feff}: keepalive\n\n",
                "id: untrusted-id\nevent: message\nunknown: ignored\n",
                "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"雪\"}}],\"usage\":null}\n\n",
                "data: {\"object\":\"chat.completion.chunk\",\"choices\":[],\n",
                "data: \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\n",
                "data: [DONE]\n\n"
            )
            .replace('\n', newline);
            for split in 0..=wire.len() {
                let mut observer = OpenAiStreamUsageObserver::new();
                observer.push(&wire.as_bytes()[..split]).unwrap();
                observer.push(&wire.as_bytes()[split..]).unwrap();
                assert!(observer.is_done());
                assert_eq!(observer.usage().unwrap().tokens_in, 10);
                assert_eq!(observer.error(), None);
                assert_eq!(observer.finish(), Ok(()));
                assert_eq!(observer.finish(), Ok(()));
                assert_eq!(
                    observer.push(b"new response"),
                    Err(StreamObservationError::Closed)
                );
            }
            let mut observer = OpenAiStreamUsageObserver::new();
            for byte in wire.bytes() {
                observer.push(&[byte]).unwrap();
            }
            assert!(observer.is_done());
            assert!(observer.usage().is_some());
        }
    }

    #[test]
    fn stream_observer_never_promotes_partial_events_or_done_to_usage() {
        let final_event = event(10);
        for trim in [1, 2, 10] {
            let mut observer = OpenAiStreamUsageObserver::new();
            observer
                .push(&final_event.as_bytes()[..final_event.len() - trim])
                .unwrap();
            assert_eq!(observer.usage(), None);
            assert_eq!(
                observer.finish(),
                Err(StreamObservationError::TruncatedEvent)
            );
            assert_eq!(observer.usage(), None);
        }
        let mut observer = OpenAiStreamUsageObserver::new();
        observer.push(final_event.as_bytes()).unwrap();
        let usage = observer.usage();
        assert!(usage.is_some());
        assert!(!observer.is_done());
        assert_eq!(
            observer.finish(),
            Err(StreamObservationError::MissingTerminator)
        );
        assert_eq!(observer.usage(), usage);

        let mut observer = OpenAiStreamUsageObserver::new();
        observer.push(b"data: [DONE]\r\r").unwrap();
        assert!(
            observer.is_done(),
            "CR dispatch must not wait for another byte"
        );
        assert_eq!(observer.usage(), None);
        assert_eq!(observer.finish(), Ok(()));
    }

    #[test]
    fn stream_observer_retains_first_usage_and_reports_conflicts_or_later_failure() {
        let cache_changed = format!(
            "data: {}\n\n",
            json!({"object":"chat.completion.chunk","choices":[],
            "usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":0}}})
        );
        for (suffix, expected) in [
            (event(11), StreamObservationError::ConflictingUsage),
            (cache_changed, StreamObservationError::ConflictingUsage),
            (
                "data: invalid\n\n".into(),
                StreamObservationError::InvalidJson,
            ),
            (
                "data: partial".into(),
                StreamObservationError::TruncatedEvent,
            ),
        ] {
            let mut observer = OpenAiStreamUsageObserver::new();
            observer.push(event(10).as_bytes()).unwrap();
            let first = observer.usage();
            observer.push(event(10).as_bytes()).unwrap(); // Identical observation is idempotent.
            let error = observer
                .push(suffix.as_bytes())
                .err()
                .or_else(|| observer.finish().err())
                .unwrap();
            assert_eq!(error, expected);
            assert_eq!(observer.error(), Some(error));
            assert_eq!(observer.usage(), first);
            assert_eq!(observer.push(event(12).as_bytes()), Err(error));
            assert_eq!(observer.finish(), Err(error));
            assert_eq!(observer.usage(), first);
        }
    }

    #[test]
    fn stream_observer_rejects_invalid_events_and_post_done_data() {
        for (wire, error) in [
            (
                b"data: \xff\n\n".to_vec(),
                StreamObservationError::InvalidUtf8,
            ),
            (
                b"data: invalid\n\n".to_vec(),
                StreamObservationError::InvalidJson,
            ),
            (
                b"data: [DONE]\ndata: more\n\n".to_vec(),
                StreamObservationError::InvalidJson,
            ),
            (
                [b"event: error\n".as_slice(), event(10).as_bytes()].concat(),
                StreamObservationError::UnexpectedEventType,
            ),
            (
                [b"event: usage\n".as_slice(), event(10).as_bytes()].concat(),
                StreamObservationError::UnexpectedEventType,
            ),
            (
                [b": first\n\n\xef\xbb\xbf".as_slice(), event(10).as_bytes()].concat(),
                StreamObservationError::UnexpectedBom,
            ),
            (
                b"data: {}\n\n".to_vec(),
                StreamObservationError::InvalidUsage(
                    sandhi_core::usage::StreamUsageError::InvalidChunk,
                ),
            ),
        ] {
            let mut observer = OpenAiStreamUsageObserver::new();
            assert_eq!(observer.push(&wire), Err(error));
            assert_eq!(observer.usage(), None);
            assert_eq!(observer.error(), Some(error));
        }
        let mut observer = OpenAiStreamUsageObserver::new();
        observer.push(event(10).as_bytes()).unwrap();
        let first = observer.usage();
        observer.push(b"data: [DONE]\n\n").unwrap();
        assert_eq!(
            observer.push(event(11).as_bytes()),
            Err(StreamObservationError::DataAfterTerminator)
        );
        assert_eq!(observer.usage(), first);
    }

    #[test]
    fn stream_observer_bounds_complete_pending_and_aggregate_events_without_salvage() {
        for wire in [
            vec![b'x'; 20_000],
            [vec![b'x'; 20_000], b"\n\n".to_vec()].concat(),
            [vec![b'x'; 600], b"\n\n".to_vec()].concat(),
        ] {
            let mut observer = OpenAiStreamUsageObserver::with_limit(512);
            observer.push(event(10).as_bytes()).unwrap();
            let first = observer.usage();
            assert_eq!(
                observer.push(&wire),
                Err(StreamObservationError::LineTooLong)
            );
            assert!(observer.peak_buffered <= 512 + INPUT_SLICE_BYTES);
            assert_eq!(
                observer.push(event(11).as_bytes()),
                Err(StreamObservationError::LineTooLong)
            );
            assert_eq!(observer.usage(), first);
        }
        for line in [
            ": comment padding\n",
            "unknown: ignored padding\n",
            "data: lots of padding\n",
        ] {
            let mut observer = OpenAiStreamUsageObserver::with_limit(512);
            assert_eq!(
                observer.push(line.repeat(100).as_bytes()),
                Err(StreamObservationError::EventTooLong)
            );
            assert_eq!(observer.usage(), None);
            assert!(observer.data.len() <= 512);
        }
    }
}
