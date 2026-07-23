//! Typed provider runtime and OpenAI-compatible codec.

use crate::{
    Anthropic, AnthropicAuthScheme, Attribution, ByteStream, Cohere, Gemini, GeminiAuthScheme,
    Ollama, OpenAiCompat, OpenAiResponses, OpenAiResponsesProfile, ParsedUsage, Provider,
    ProviderError, ProviderRequest, ResilientProvider, TimeoutConfig,
};
use async_trait::async_trait;
use futures_core::Stream;
use sandhi_core::{
    AssistantOutputV1, ChatMessageV1, ChatRequestV1, ChatResponseV1, ChatStreamEventV1,
    ContentPart, FinishReasonV1, MessageContent, ProviderErrorV1, ToolCallV1, ToolChoiceMode,
    ToolChoiceV1, UsageCompleteness, UsageV2,
};
use serde_json::{json, Map, Value};
use std::{collections::BTreeMap, pin::Pin, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFamily {
    OpenAiCompat,
    OpenAiResponses,
    Anthropic,
    Cohere,
    Gemini,
    Ollama,
}

impl ProviderFamily {
    #[must_use]
    pub fn for_slug(slug: &str) -> Self {
        match slug {
            "anthropic" => Self::Anthropic,
            "cohere" => Self::Cohere,
            "gemini" => Self::Gemini,
            "ollama" => Self::Ollama,
            _ => Self::OpenAiCompat,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderTransportConfig {
    pub family: ProviderFamily,
    pub slug: String,
    pub base_url: String,
    pub api_key: String,
    pub headers: reqwest::header::HeaderMap,
    pub max_retries: Option<u32>,
    pub timeout_secs: Option<f64>,
    pub stream_idle_timeout_secs: Option<f64>,
    pub anthropic_auth_scheme: AnthropicAuthScheme,
    pub gemini_auth_scheme: GeminiAuthScheme,
    pub openai_responses_profile: OpenAiResponsesProfile,
}

impl ProviderTransportConfig {
    #[must_use]
    pub fn new(
        family: ProviderFamily,
        slug: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            family,
            slug: slug.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            headers: reqwest::header::HeaderMap::new(),
            max_retries: None,
            timeout_secs: None,
            stream_idle_timeout_secs: None,
            anthropic_auth_scheme: AnthropicAuthScheme::ApiKey,
            gemini_auth_scheme: GeminiAuthScheme::ApiKey,
            openai_responses_profile: OpenAiResponsesProfile::Standard,
        }
    }
}

pub type ChatEventStream =
    Pin<Box<dyn Stream<Item = Result<ChatStreamEventV1, ProviderError>> + Send>>;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    fn slug(&self) -> &str;
    async fn complete(&self, request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError>;
    async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError>;
}

#[derive(Clone)]
pub struct ProviderHandle {
    inner: Arc<dyn ChatProvider>,
    /// The vault-declared / config-declared family (TD-0006 / ADR-0004 D1). This is what the
    /// proxy's plane-selection will use to decide transparent-forward vs. cross-family
    /// translation. It is set from the factory constructor (config-driven), **not** from
    /// [`ProviderFamily::for_slug`] (which defaults unknown slugs to OpenAI-compat and would
    /// byte-forward an OpenAI body to an Anthropic upstream).
    family: ProviderFamily,
}

impl ProviderHandle {
    /// Wrap a typed provider implementation in a persistent handle.
    ///
    /// This is the typed extension seam used by gateway tests and host-owned providers. Raw
    /// provider-native request/response transports intentionally do not cross this boundary.
    /// The family defaults to [`ProviderFamily::OpenAiCompat`]; use [`with_family`] to override
    /// for non-OpenAI providers constructed via this escape hatch.
    ///
    /// [`with_family`]: Self::with_family
    #[must_use]
    pub fn new(inner: Arc<dyn ChatProvider>) -> Self {
        Self {
            inner,
            family: ProviderFamily::OpenAiCompat,
        }
    }

    /// Declare the provider family on a handle constructed via [`new`]. For handles created
    /// through the [`ProviderRuntime`] factory methods, the family is already set from config.
    ///
    /// [`new`]: Self::new
    #[must_use]
    pub fn with_family(mut self, family: ProviderFamily) -> Self {
        self.family = family;
        self
    }

    /// The vault-declared / config-declared family — **not** slug-derived. Proxy
    /// plane-selection (TD-0006 Step 2) uses this to decide whether to forward raw bytes
    /// (same-family transparent plane) or route through `ChatRequestV1` translation
    /// (cross-family plane). A custom-slug row must resolve by CONFIG, not by
    /// [`ProviderFamily::for_slug`].
    #[must_use]
    pub fn family(&self) -> ProviderFamily {
        self.family
    }

    pub fn slug(&self) -> &str {
        self.inner.slug()
    }

    pub async fn complete(&self, request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
        self.inner.complete(request).await
    }

    pub async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        self.inner.stream(request).await
    }
}

/// Factory for persistent typed provider handles. A handle owns one adapter and therefore one
/// HTTP connection pool, circuit breaker, and retry policy across all of its calls.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProviderRuntime;

impl ProviderRuntime {
    pub fn new() -> Self {
        Self
    }

    /// Construct the one resilient provider transport used internally by typed handles,
    /// bindings, and the proxy. Provider-native JSON does not cross a public binding boundary.
    #[must_use]
    pub fn transport(&self, config: ProviderTransportConfig) -> Arc<dyn Provider> {
        let bare: Arc<dyn Provider> = match config.family {
            ProviderFamily::OpenAiCompat => Arc::new(
                OpenAiCompat::new(
                    config.slug.clone(),
                    config.base_url.clone(),
                    config.api_key.clone(),
                )
                .with_headers(config.headers.clone()),
            ),
            ProviderFamily::OpenAiResponses => Arc::new(
                OpenAiResponses::new(
                    config.slug.clone(),
                    config.base_url.clone(),
                    config.api_key.clone(),
                )
                .with_headers(config.headers.clone())
                .with_profile(config.openai_responses_profile),
            ),
            ProviderFamily::Anthropic => Arc::new(
                Anthropic::new(config.base_url.clone(), config.api_key.clone())
                    .with_auth_scheme(config.anthropic_auth_scheme),
            ),
            ProviderFamily::Cohere => {
                Arc::new(Cohere::new(config.base_url.clone(), config.api_key.clone()))
            }
            ProviderFamily::Gemini => Arc::new(
                Gemini::new(config.base_url.clone(), config.api_key.clone())
                    .with_auth_scheme(config.gemini_auth_scheme),
            ),
            ProviderFamily::Ollama => {
                let provider = Ollama::new(config.base_url.clone());
                if config.api_key.is_empty() {
                    Arc::new(provider)
                } else {
                    Arc::new(provider.with_api_key(config.api_key.clone()))
                }
            }
        };
        self.decorate_transport(bare, &config)
    }

    /// Apply the runtime's resilience policy to a host-provided transport escape hatch.
    #[must_use]
    pub fn decorate_transport(
        &self,
        bare: Arc<dyn Provider>,
        config: &ProviderTransportConfig,
    ) -> Arc<dyn Provider> {
        let mut resilient = ResilientProvider::new(bare);
        if let Some(max_retries) = config.max_retries {
            resilient = resilient.with_retry(max_retries, std::time::Duration::from_millis(200));
        }
        let mut timeouts = TimeoutConfig::default();
        if let Some(secs) = config.timeout_secs {
            timeouts.complete = std::time::Duration::from_secs_f64(secs.max(0.001));
        }
        if let Some(secs) = config.stream_idle_timeout_secs {
            timeouts.idle = Some(std::time::Duration::from_secs_f64(secs.max(0.001)));
        }
        Arc::new(resilient.with_timeouts(timeouts))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn openai_compat(
        &self,
        slug: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        headers: reqwest::header::HeaderMap,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> ProviderHandle {
        let slug = slug.into();
        let mut config = ProviderTransportConfig::new(
            ProviderFamily::OpenAiCompat,
            slug.clone(),
            base_url,
            api_key,
        );
        config.headers = headers;
        config.max_retries = max_retries;
        config.timeout_secs = timeout_secs;
        config.stream_idle_timeout_secs = stream_idle_timeout_secs;
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(TypedOpenAiCompat { slug, raw }),
            family: ProviderFamily::OpenAiCompat,
        }
    }

    /// Construct a typed OpenAI Responses handle. This is a distinct item/event protocol and
    /// must be selected explicitly; credentials or endpoint strings never imply a protocol.
    #[allow(clippy::too_many_arguments)]
    pub fn openai_responses(
        &self,
        slug: impl Into<String>,
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
        headers: reqwest::header::HeaderMap,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> ProviderHandle {
        let slug = slug.into();
        let mut config = ProviderTransportConfig::new(
            ProviderFamily::OpenAiResponses,
            slug.clone(),
            base_url,
            bearer_token,
        );
        config.headers = headers;
        config.max_retries = max_retries;
        config.timeout_secs = timeout_secs;
        config.stream_idle_timeout_secs = stream_idle_timeout_secs;
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::openai_responses_typed::TypedOpenAiResponses::new(
                slug,
                raw,
                OpenAiResponsesProfile::Standard,
            )),
            family: ProviderFamily::OpenAiResponses,
        }
    }

    /// ChatGPT subscription Responses profile. The upstream is SSE-only; completed calls are
    /// aggregated from the same typed event stream so the host still sees `ChatResponseV1`.
    #[allow(clippy::too_many_arguments)]
    pub fn chatgpt_responses(
        &self,
        slug: impl Into<String>,
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
        headers: reqwest::header::HeaderMap,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> ProviderHandle {
        let slug = slug.into();
        let mut config = ProviderTransportConfig::new(
            ProviderFamily::OpenAiResponses,
            slug.clone(),
            base_url,
            bearer_token,
        );
        config.headers = headers;
        config.max_retries = max_retries;
        config.timeout_secs = timeout_secs;
        config.stream_idle_timeout_secs = stream_idle_timeout_secs;
        config.openai_responses_profile = OpenAiResponsesProfile::ChatGptCodex;
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::openai_responses_typed::TypedOpenAiResponses::new(
                slug,
                raw,
                OpenAiResponsesProfile::ChatGptCodex,
            )),
            family: ProviderFamily::OpenAiResponses,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn anthropic(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        auth_scheme: AnthropicAuthScheme,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> ProviderHandle {
        let mut config =
            ProviderTransportConfig::new(ProviderFamily::Anthropic, "anthropic", base_url, api_key);
        config.anthropic_auth_scheme = auth_scheme;
        config.max_retries = max_retries;
        config.timeout_secs = timeout_secs;
        config.stream_idle_timeout_secs = stream_idle_timeout_secs;
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::anthropic_typed::TypedAnthropic::new(raw)),
            family: ProviderFamily::Anthropic,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ollama(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> ProviderHandle {
        let mut config =
            ProviderTransportConfig::new(ProviderFamily::Ollama, "ollama", base_url, api_key);
        config.max_retries = max_retries;
        config.timeout_secs = timeout_secs;
        config.stream_idle_timeout_secs = stream_idle_timeout_secs;
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::ollama_typed::TypedOllama::new(raw)),
            family: ProviderFamily::Ollama,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn gemini(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        auth_scheme: GeminiAuthScheme,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> ProviderHandle {
        let mut config =
            ProviderTransportConfig::new(ProviderFamily::Gemini, "gemini", base_url, api_key);
        config.gemini_auth_scheme = auth_scheme;
        config.max_retries = max_retries;
        config.timeout_secs = timeout_secs;
        config.stream_idle_timeout_secs = stream_idle_timeout_secs;
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::gemini_typed::TypedGemini::new(raw)),
            family: ProviderFamily::Gemini,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cohere(
        &self,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> ProviderHandle {
        let mut config =
            ProviderTransportConfig::new(ProviderFamily::Cohere, "cohere", base_url, api_key);
        config.max_retries = max_retries;
        config.timeout_secs = timeout_secs;
        config.stream_idle_timeout_secs = stream_idle_timeout_secs;
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::cohere_typed::TypedCohere::new(raw)),
            family: ProviderFamily::Cohere,
        }
    }

    /// Create a handle from Sandhi's authoritative provider catalog. The explicit
    /// [`Self::openai_compat`] constructor remains the escape hatch for custom endpoints.
    #[allow(clippy::too_many_arguments)]
    pub fn known_openai_compat(
        &self,
        provider: &str,
        model: &str,
        api_key: impl Into<String>,
        headers: reqwest::header::HeaderMap,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> Result<ProviderHandle, ProviderError> {
        let spec = crate::resolve_openai_compat_provider(provider).ok_or_else(|| {
            ProviderError::InvalidRequest(format!("unknown catalog provider: {provider}"))
        })?;
        Ok(self.openai_compat(
            spec.slug,
            spec.base_url_for_model(model),
            api_key,
            headers,
            max_retries,
            timeout_secs,
            stream_idle_timeout_secs,
        ))
    }
}

struct TypedOpenAiCompat {
    slug: String,
    raw: Arc<dyn Provider>,
}

#[async_trait]
impl ChatProvider for TypedOpenAiCompat {
    fn slug(&self) -> &str {
        &self.slug
    }

    async fn complete(&self, mut request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
        self.apply_constraints(&mut request)?;
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let req = provider_request(&request, encode_openai_request(&request)?);
        let response = self.raw.complete(req).await?;
        let mut decoded = decode_openai_response(response.body, response.usage, &request.model)?;
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        Ok(decoded)
    }

    async fn stream(&self, mut request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        self.apply_constraints(&mut request)?;
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let req = provider_request(&request, encode_openai_request(&request)?);
        let raw = self.raw.stream(req).await?;
        Ok(decode_openai_stream(raw, request.model))
    }
}

impl TypedOpenAiCompat {
    fn apply_constraints(&self, request: &mut ChatRequestV1) -> Result<(), ProviderError> {
        if self.slug == "moonshot" && request.model.starts_with("kimi-k3") {
            // Kimi K3's sampling contract requires temperature=1. The host-facing default is
            // intentionally normalized here so every FFI/proxy caller gets identical behavior.
            request.temperature = Some(1.0);
            if let Some(effort) = request
                .extensions
                .get("openai")
                .and_then(|value| value.get("reasoning_effort"))
                .and_then(Value::as_str)
            {
                if !matches!(effort, "low" | "high" | "max") {
                    return Err(ProviderError::InvalidRequest(format!(
                        "Kimi K3 reasoning_effort must be low, high, or max; got {effort}"
                    )));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn provider_request(request: &ChatRequestV1, body: Value) -> ProviderRequest {
    ProviderRequest::new(request.model.clone(), body)
        .with_session(request.metadata.session_id.clone())
        .with_attribution(Attribution {
            virtual_key_id: request.metadata.virtual_key_id.clone(),
            subject_id: request.metadata.subject_id.clone(),
            group_id: request.metadata.group_id.clone(),
            route: request.metadata.route.clone(),
        })
}

pub fn encode_openai_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request.validate().map_err(ProviderError::InvalidRequest)?;
    let mut body = request
        .extensions
        .get("openai")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert(
        "messages".into(),
        Value::Array(request.messages.iter().map(encode_message).collect()),
    );
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({"type":"function", "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                            "strict": tool.strict,
                        }})
                    })
                    .collect(),
            ),
        );
    }
    if let Some(choice) = &request.tool_choice {
        body.insert("tool_choice".into(), encode_tool_choice(choice));
    }
    insert_optional(&mut body, "temperature", request.temperature);
    insert_optional(&mut body, "max_tokens", request.max_output_tokens);
    insert_optional(&mut body, "seed", request.seed);
    if let Some(stop) = &request.stop {
        body.insert("stop".into(), json!(stop));
    }
    if let Some(format) = &request.response_format {
        body.insert("response_format".into(), format.clone());
    }
    Ok(Value::Object(body))
}

fn insert_optional<T: serde::Serialize>(
    body: &mut Map<String, Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value {
        if let Ok(value) = serde_json::to_value(value) {
            body.insert(key.into(), value);
        }
    }
}

fn encode_message(message: &ChatMessageV1) -> Value {
    match message {
        ChatMessageV1::Developer { content, name } => {
            message_with_content("developer", content, name.as_deref())
        }
        ChatMessageV1::System { content, name } => {
            message_with_content("system", content, name.as_deref())
        }
        ChatMessageV1::User { content, name } => {
            message_with_content("user", content, name.as_deref())
        }
        ChatMessageV1::Assistant {
            content,
            name,
            tool_calls,
            refusal,
        } => {
            let mut out = Map::from_iter([("role".into(), Value::String("assistant".into()))]);
            if let Some(content) = content {
                out.insert("content".into(), encode_content(content));
            }
            if let Some(name) = name {
                out.insert("name".into(), Value::String(name.clone()));
            }
            if !tool_calls.is_empty() {
                out.insert(
                    "tool_calls".into(),
                    Value::Array(tool_calls.iter().map(encode_tool_call).collect()),
                );
            }
            if let Some(refusal) = refusal {
                out.insert("refusal".into(), Value::String(refusal.clone()));
            }
            Value::Object(out)
        }
        ChatMessageV1::Tool {
            content,
            tool_call_id,
        } => json!({"role":"tool", "content":encode_content(content), "tool_call_id":tool_call_id}),
        ChatMessageV1::Function { content, name } => {
            json!({"role":"function", "content":encode_content(content), "name":name})
        }
    }
}

fn message_with_content(role: &str, content: &MessageContent, name: Option<&str>) -> Value {
    let mut out = Map::from_iter([
        ("role".into(), Value::String(role.into())),
        ("content".into(), encode_content(content)),
    ]);
    if let Some(name) = name {
        out.insert("name".into(), Value::String(name.into()));
    }
    Value::Object(out)
}

fn encode_content(content: &MessageContent) -> Value {
    match content {
        MessageContent::Text(text) => Value::String(text.clone()),
        MessageContent::Parts(parts) => {
            Value::Array(parts.iter().map(encode_content_part).collect())
        }
    }
}

fn encode_content_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({"type":"text", "text":text}),
        ContentPart::ImageUrl { image_url, detail } => {
            json!({"type":"image_url", "image_url":{"url":image_url, "detail":detail}})
        }
        ContentPart::InputAudio { data, format } => {
            json!({"type":"input_audio", "input_audio":{"data":data, "format":format}})
        }
        ContentPart::File {
            file_id,
            file_data,
            filename,
        } => {
            json!({"type":"file", "file":{"file_id":file_id,"file_data":file_data,"filename":filename}})
        }
    }
}

fn encode_tool_call(call: &ToolCallV1) -> Value {
    json!({"id":call.id, "type":"function", "function":{"name":call.name,"arguments":call.arguments}})
}

fn encode_tool_choice(choice: &ToolChoiceV1) -> Value {
    match choice {
        ToolChoiceV1::Mode(ToolChoiceMode::None) => Value::String("none".into()),
        ToolChoiceV1::Mode(ToolChoiceMode::Auto) => Value::String("auto".into()),
        ToolChoiceV1::Mode(ToolChoiceMode::Required) => Value::String("required".into()),
        ToolChoiceV1::Function { name } => {
            json!({"type":"function", "function":{"name":name}})
        }
    }
}

pub fn decode_openai_response(
    body: Value,
    parsed_usage: ParsedUsage,
    requested_model: &str,
) -> Result<ChatResponseV1, ProviderError> {
    let message = body
        .pointer("/choices/0/message")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            ProviderError::Transport("OpenAI response has no choices[0].message".into())
        })?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .map(|s| MessageContent::Text(s.into()));
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().filter_map(decode_tool_call).collect())
        .unwrap_or_default();
    let usage = usage_v2_from_openai(&body, parsed_usage, UsageCompleteness::Final);
    let extensions = BTreeMap::from([("openai".into(), body.clone())]);
    Ok(ChatResponseV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(requested_model)
            .into(),
        output: AssistantOutputV1 {
            content,
            tool_calls,
            refusal: message
                .get("refusal")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        finish_reason: body
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .map(decode_finish_reason),
        usage,
        // Preserve the provider-native response for compatibility/debugging consumers without
        // polluting the neutral fields. Hosts must not depend on this for shaped semantics.
        extensions,
    })
}

fn decode_tool_call(value: &Value) -> Option<ToolCallV1> {
    Some(ToolCallV1 {
        id: value.get("id")?.as_str()?.into(),
        name: value.pointer("/function/name")?.as_str()?.into(),
        arguments: value
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        extensions: BTreeMap::new(),
    })
}

fn decode_finish_reason(reason: &str) -> FinishReasonV1 {
    match reason {
        "stop" => FinishReasonV1::Stop,
        "length" => FinishReasonV1::Length,
        "tool_calls" => FinishReasonV1::ToolCalls,
        "content_filter" => FinishReasonV1::ContentFilter,
        "function_call" => FinishReasonV1::FunctionCall,
        _ => FinishReasonV1::Unknown,
    }
}

fn usage_v2_from_openai(
    body: &Value,
    parsed: ParsedUsage,
    completeness: UsageCompleteness,
) -> UsageV2 {
    let usage = body.get("usage").unwrap_or(&Value::Null);
    let prompt_details = usage.get("prompt_tokens_details").unwrap_or(&Value::Null);
    let completion_details = usage
        .get("completion_tokens_details")
        .unwrap_or(&Value::Null);
    UsageV2 {
        completeness,
        audio_input_tokens: u64_opt(prompt_details, "audio_tokens"),
        audio_output_tokens: u64_opt(completion_details, "audio_tokens"),
        reasoning_tokens: u64_opt(completion_details, "reasoning_tokens"),
        accepted_prediction_tokens: u64_opt(completion_details, "accepted_prediction_tokens"),
        rejected_prediction_tokens: u64_opt(completion_details, "rejected_prediction_tokens"),
        ..parsed.into()
    }
}

fn u64_opt(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn decode_openai_stream(mut raw: ByteStream, requested_model: String) -> ChatEventStream {
    use futures_util::StreamExt;
    let stream = async_stream::try_stream! {
        let mut buffer = Vec::<u8>::new();
        let mut started = false;
        let mut open_tools = BTreeMap::<u32, ()>::new();
        let mut emitted_usage = false;
        while let Some(chunk) = raw.next().await {
            let chunk = chunk?;
            let attempts = chunk.attempts;
            if !chunk.data.is_empty() {
                buffer.extend_from_slice(&chunk.data);
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let Some(value) = crate::sse_data_json(&line) else { continue; };
                    if !started {
                        yield ChatStreamEventV1::ResponseStart {
                            id: value.get("id").and_then(Value::as_str).map(str::to_owned),
                            model: value.get("model").and_then(Value::as_str)
                                .unwrap_or(&requested_model).to_owned(),
                        };
                        started = true;
                    }
                    if let Some(delta) = value.pointer("/choices/0/delta") {
                        if let Some(text) = delta.get("content").and_then(Value::as_str) {
                            yield ChatStreamEventV1::TextDelta { delta: text.into() };
                        }
                        if let Some(text) = delta.get("reasoning_content")
                            .or_else(|| delta.get("reasoning")).and_then(Value::as_str) {
                            yield ChatStreamEventV1::ReasoningDelta { delta: text.into() };
                        }
                        if let Some(text) = delta.get("refusal").and_then(Value::as_str) {
                            yield ChatStreamEventV1::RefusalDelta { delta: text.into() };
                        }
                        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                            for call in calls {
                                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                                if let std::collections::btree_map::Entry::Vacant(entry) = open_tools.entry(index) {
                                    if let (Some(id), Some(name)) = (
                                        call.get("id").and_then(Value::as_str),
                                        call.pointer("/function/name").and_then(Value::as_str),
                                    ) {
                                        entry.insert(());
                                        yield ChatStreamEventV1::ToolCallStart {
                                            index, id: id.into(), name: name.into()
                                        };
                                    }
                                }
                                if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
                                    if !arguments.is_empty() {
                                        yield ChatStreamEventV1::ToolCallArgumentsDelta { index, delta: arguments.into() };
                                    }
                                }
                            }
                        }
                    }
                    if value.get("usage").is_some_and(|usage| !usage.is_null()) {
                        let parsed = crate::parse_openai_usage(&value).unwrap_or_default();
                        let mut usage = usage_v2_from_openai(&value, parsed, UsageCompleteness::Final);
                        usage.attempts = attempts;
                        usage.outcome = Some("success".into());
                        yield ChatStreamEventV1::Usage { usage };
                        emitted_usage = true;
                    }
                    if let Some(reason) = value.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
                        for index in open_tools.keys().copied().collect::<Vec<_>>() {
                            yield ChatStreamEventV1::ToolCallEnd { index };
                        }
                        open_tools.clear();
                        yield ChatStreamEventV1::Finish { reason: decode_finish_reason(reason) };
                    }
                }
            }
            if let Some(usage) = chunk.usage {
                if !emitted_usage {
                    let mut usage: UsageV2 = usage.into();
                    usage.completeness = UsageCompleteness::Final;
                    usage.attempts = attempts;
                    usage.outcome = Some("success".into());
                    yield ChatStreamEventV1::Usage { usage };
                    emitted_usage = true;
                }
            }
        }
    };
    Box::pin(stream)
}

impl ProviderError {
    pub fn as_typed(&self, provider: Option<&str>) -> ProviderErrorV1 {
        let (code, retryable, http_status) = match self {
            Self::InvalidRequest(_) => ("invalid_request", false, Some(400)),
            Self::Auth => ("authentication_error", false, Some(401)),
            Self::RateLimited => ("rate_limited", true, Some(429)),
            Self::Upstream(status) => ("upstream_error", *status >= 500, Some(*status)),
            Self::Transport(_) => ("transport_error", true, None),
            Self::CircuitOpen => ("circuit_open", true, Some(503)),
            Self::Timeout(_) => ("timeout", true, Some(504)),
        };
        ProviderErrorV1 {
            code: code.into(),
            message: self.to_string(),
            retryable,
            http_status,
            provider: provider.map(str::to_owned),
            request_id: None,
            details: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;

    fn request() -> ChatRequestV1 {
        serde_json::from_value(json!({
            "model":"gpt-test",
            "messages":[
                {"role":"developer","content":"be precise"},
                {"role":"user","content":[
                    {"type":"text","text":"look"},
                    {"type":"image_url","image_url":"https://example.test/a.png","detail":"low"}
                ]},
                {"role":"assistant","tool_calls":[{"id":"c1","name":"lookup","arguments":"{\"q\":1}"}]},
                {"role":"tool","content":"done","tool_call_id":"c1"}
            ],
            "tools":[{"name":"lookup","parameters":{"type":"object"}}],
            "tool_choice":{"name":"lookup"},
            "max_output_tokens":42,
            "extensions":{"openai":{"top_p":0.8}}
        })).unwrap()
    }

    #[test]
    fn openai_encoder_preserves_roles_parts_tools_and_extensions() {
        let body = encode_openai_request(&request()).unwrap();
        assert_eq!(body["messages"][0]["role"], "developer");
        assert_eq!(
            body["messages"][1]["content"][1]["image_url"]["url"],
            "https://example.test/a.png"
        );
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["max_tokens"], 42);
        assert_eq!(body["top_p"], 0.8);
    }

    #[test]
    fn moonshot_k3_constraint_is_single_sourced_in_the_typed_codec() {
        let raw: Arc<dyn Provider> = Arc::new(crate::FnProvider::new("moonshot", |_req| async {
            unreachable!()
        }));
        let provider = TypedOpenAiCompat {
            slug: "moonshot".into(),
            raw,
        };
        let mut request = request();
        request.model = "kimi-k3".into();
        request.temperature = Some(0.7);
        provider.apply_constraints(&mut request).unwrap();
        assert_eq!(request.temperature, Some(1.0));

        request
            .extensions
            .insert("openai".into(), json!({"reasoning_effort":"medium"}));
        assert!(provider.apply_constraints(&mut request).is_err());
    }

    #[test]
    fn openai_decoder_retains_refusal_tool_calls_finish_and_detailed_usage() {
        let body = json!({
            "id":"r1", "model":"gpt-test",
            "choices":[{"message":{"content":null,"refusal":"no","tool_calls":[
                {"id":"c1","type":"function","function":{"name":"lookup","arguments":"{}"}}
            ]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,
                "prompt_tokens_details":{"cached_tokens":4,"audio_tokens":2},
                "completion_tokens_details":{"reasoning_tokens":3}}
        });
        let parsed = crate::parse_openai_usage(&body).unwrap();
        let out = decode_openai_response(body, parsed, "fallback").unwrap();
        assert_eq!(out.output.refusal.as_deref(), Some("no"));
        assert_eq!(out.output.tool_calls[0].name, "lookup");
        assert_eq!(out.finish_reason, Some(FinishReasonV1::ToolCalls));
        assert_eq!(out.usage.tokens_in, 6);
        assert_eq!(out.usage.audio_input_tokens, Some(2));
        assert_eq!(out.usage.reasoning_tokens, Some(3));
    }

    #[tokio::test]
    async fn stream_codec_is_invariant_across_arbitrary_byte_boundaries() {
        let sse = concat!(
            "data: {\"id\":\"r1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n"
        ).as_bytes();
        for split in 0..=sse.len() {
            let raw: ByteStream = Box::pin(futures_util::stream::iter(vec![
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse[..split]),
                    usage: None,
                    attempts: 3,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse[split..]),
                    usage: None,
                    attempts: 3,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::new(),
                    usage: Some(ParsedUsage {
                        tokens_in: 6,
                        tokens_out: 5,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 4,
                    }),
                    attempts: 3,
                }),
            ]));
            let events = decode_openai_stream(raw, "fallback".into())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(
                matches!(
                    events.first(),
                    Some(ChatStreamEventV1::ResponseStart { .. })
                ),
                "split {split}"
            );
            assert!(events.iter().any(|event| matches!(event, ChatStreamEventV1::ToolCallStart { index: 0, id, .. } if id == "c1")), "split {split}");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ChatStreamEventV1::ToolCallEnd { index: 0 })),
                "split {split}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ChatStreamEventV1::Usage { .. }))
                    .count(),
                1,
                "split {split}"
            );
            assert!(events.iter().any(|event| matches!(
                event,
                ChatStreamEventV1::Usage { usage } if usage.attempts == 3
            )));
        }
    }

    // -----------------------------------------------------------------------------------------
    // ProviderFamily accessor (TD-0006 / ADR-0004 D1)
    // -----------------------------------------------------------------------------------------

    #[test]
    fn family_accessor_returns_config_declared_family_from_factory() {
        let runtime = ProviderRuntime::new();
        // Each factory constructor stamps the family from config, not from slug.
        let openai = runtime.openai_compat(
            "openai",
            "https://api.openai.com/v1",
            "k",
            HeaderMap::new(),
            None,
            None,
            None,
        );
        assert_eq!(openai.family(), ProviderFamily::OpenAiCompat);

        let anthropic = runtime.anthropic(
            "https://api.anthropic.com",
            "k",
            crate::AnthropicAuthScheme::ApiKey,
            None,
            None,
            None,
        );
        assert_eq!(anthropic.family(), ProviderFamily::Anthropic);

        let gemini = runtime.gemini(
            "https://generativelanguage.googleapis.com",
            "k",
            crate::GeminiAuthScheme::ApiKey,
            None,
            None,
            None,
        );
        assert_eq!(gemini.family(), ProviderFamily::Gemini);

        let cohere = runtime.cohere("https://api.cohere.ai", "k", None, None, None);
        assert_eq!(cohere.family(), ProviderFamily::Cohere);
    }

    #[test]
    fn custom_slug_resolves_family_by_config_not_slug_heuristic() {
        // A custom-slug endpoint configured as Anthropic must resolve as Anthropic — NOT
        // as OpenAiCompat (which for_slug would return for an unknown slug).
        let runtime = ProviderRuntime::new();
        let custom = runtime.anthropic(
            "https://internal-llm.corp.example",
            "k",
            crate::AnthropicAuthScheme::ApiKey,
            None,
            None,
            None,
        );
        // The factory sets family from the constructor (config), not from the slug.
        assert_eq!(custom.family(), ProviderFamily::Anthropic);
        // for_slug would default to OpenAiCompat — wrong for a custom Anthropic endpoint.
        assert_eq!(
            ProviderFamily::for_slug("internal-llm"),
            ProviderFamily::OpenAiCompat
        );
        // The config-declared family is the authoritative answer.
        assert_ne!(custom.family(), ProviderFamily::for_slug("internal-llm"));
    }

    #[test]
    fn handle_new_defaults_and_with_family_overrides() {
        let bare: Arc<dyn ChatProvider> = Arc::new(NoOpProvider);
        // new() defaults to OpenAiCompat for backward-compat extension seam.
        let default = ProviderHandle::new(bare.clone());
        assert_eq!(default.family(), ProviderFamily::OpenAiCompat);
        // with_family overrides for non-OpenAI providers constructed via the escape hatch.
        let gemini = ProviderHandle::new(bare).with_family(ProviderFamily::Gemini);
        assert_eq!(gemini.family(), ProviderFamily::Gemini);
    }

    /// Minimal ChatProvider mock for handle tests (never actually completes a call).
    struct NoOpProvider;
    #[async_trait]
    impl ChatProvider for NoOpProvider {
        fn slug(&self) -> &str {
            "noop"
        }
        async fn complete(&self, _: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
            unreachable!()
        }
        async fn stream(&self, _: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
            unreachable!()
        }
    }

    /// Minimal local import so the test compiles without adding a top-level `use`.
    use reqwest::header::HeaderMap;
}
