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

/// When a family reports token usage during a streaming response (TD-0013 D1).
///
/// A transport fact in the same sense as `base_url` or
/// [`OpenAiCompatProviderSpec::request_id_header`](crate::catalog::OpenAiCompatProviderSpec):
/// vendor differences are data, never branches in shared code.
///
/// The runtime does **not** switch on this. It does not need to — a family that reports nothing
/// mid-stream simply leaves [`StreamChunk::usage_running`](crate::StreamChunk::usage_running)
/// unset, so the absence of a number *is* the signal to fall back, and no caller has to know which
/// family it is talking to. What the declaration buys is the ability to state per-family behaviour
/// once, in one place, and to **test that the statement is true** rather than trusting a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageCadence {
    /// Real counts are available **while content is still arriving**, so an interrupted stream has
    /// a measurement to settle. Anthropic announces input and the full cache split on
    /// `message_start`, before any content; Gemini attaches `usageMetadata` to content chunks.
    Incremental,
    /// Nothing is reported until content is finished. A stream interrupted before that point has
    /// no number, and the fallback estimate is a *policy* choice, not an accuracy one.
    TerminalOnly,
}

/// Per-family transport facts. Deliberately one field for now: a table that grows by accretion is
/// reviewable, one that lands as a seven-site refactor of the existing `match` arms is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyFacts {
    pub usage_cadence: UsageCadence,
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

    /// The declared transport facts for this family.
    #[must_use]
    pub const fn facts(self) -> FamilyFacts {
        let usage_cadence = match self {
            // `message_start` carries input + `cache_creation_input_tokens` +
            // `cache_read_input_tokens` before a single content byte.
            Self::Anthropic => UsageCadence::Incremental,
            // `usageMetadata` rides on content-bearing chunks rather than a trailing control frame.
            Self::Gemini => UsageCadence::Incremental,
            // The usage frame arrives only once content is done: OpenAI sends it with
            // `choices: []`, Cohere on `message-end`, Ollama on the `done: true` line.
            Self::OpenAiCompat | Self::OpenAiResponses | Self::Cohere | Self::Ollama => {
                UsageCadence::TerminalOnly
            }
        };
        FamilyFacts { usage_cadence }
    }

    /// Convenience accessor for the fact that matters most often.
    #[must_use]
    pub const fn usage_cadence(self) -> UsageCadence {
        self.facts().usage_cadence
    }
}

#[derive(Debug, Clone)]
pub struct ProviderTransportConfig {
    pub family: ProviderFamily,
    pub slug: String,
    pub base_url: String,
    pub api_key: String,
    /// Extra provider headers expressed through the transport-neutral `http` contract.
    pub headers: http::HeaderMap,
    /// Extra attempts after the first. `None` and `Some(0)` are both retry-free; a positive value
    /// is an explicit assertion by the caller that replay is safe for this provider contract.
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
            headers: http::HeaderMap::new(),
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
    /// `call_headers` are per-call wire headers (TD-0022 D1): gateway-path metadata that
    /// changes per turn (`x-sandhi-run-id` / `-step-id`, vendor correlation ids). They ride
    /// the request, not the transport, because transports are cached per conversation —
    /// fixing them at construction would force a handle rebuild every turn. Every
    /// implementation must forward them; [`merge_call_headers`](crate::merge_call_headers)
    /// is the single sanctioned merge (transport-owned names stripped).
    async fn complete(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatResponseV1, ProviderError>;
    async fn stream(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatEventStream, ProviderError>;
}

/// Build the same-family raw byte-forwarder for a handle, from the **same** transport config used
/// to build its typed provider (TD-0006). Auth and headers mirror the typed path exactly, so a
/// transparent forward is credential- and header-identical to a translated one.
fn build_raw_forwarder(config: &ProviderTransportConfig) -> crate::raw::RawForwarder {
    let mut timeouts = TimeoutConfig::default();
    if let Some(secs) = config.timeout_secs {
        timeouts.complete = std::time::Duration::from_secs_f64(secs.max(0.001));
    }
    if let Some(secs) = config.stream_idle_timeout_secs {
        timeouts.idle = Some(std::time::Duration::from_secs_f64(secs.max(0.001)));
    }
    crate::raw::RawForwarder::new(
        config.family,
        config.base_url.clone(),
        config.api_key.clone(),
    )
    .with_headers(config.headers.clone())
    .with_anthropic_auth(config.anthropic_auth_scheme)
    .with_gemini_auth(config.gemini_auth_scheme)
    .with_timeouts(timeouts.complete, timeouts.stream_setup, timeouts.idle)
    .with_session_header(
        crate::resolve_openai_compat_provider(&config.slug).and_then(|spec| spec.session_header),
    )
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
    /// The raw byte-forwarder for the same-family transparent plane (TD-0006 / ADR-0004 D1), built
    /// from the **same** transport config as `inner`. `None` for handles created via the
    /// [`new`](Self::new) escape hatch (host-owned typed providers), which carry no transport
    /// config to forward with — those fall back to the typed translation path.
    raw: Option<crate::raw::RawForwarder>,
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
            raw: None,
        }
    }

    /// The raw byte-forwarder for the same-family transparent plane, or `None` for escape-hatch
    /// handles. Proxy plane-selection (TD-0006 Step 2) uses this: same-family → forward via this;
    /// cross-family or `None` → the typed `ChatRequestV1` translation path.
    #[must_use]
    pub fn raw_forwarder(&self) -> Option<&crate::raw::RawForwarder> {
        self.raw.as_ref()
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
        self.complete_with(request, http::HeaderMap::new()).await
    }

    pub async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        self.stream_with(request, http::HeaderMap::new()).await
    }

    /// [`Self::complete`] with per-call wire headers (TD-0022 D1) — the FFI seam for
    /// gateway-path metadata that changes per turn.
    pub async fn complete_with(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatResponseV1, ProviderError> {
        // Wire-truth latency, stamped once at the family-neutral typed
        // boundary (W3b) — the one seam every binding and the proxy call.
        let started = std::time::Instant::now();
        let mut response = self.inner.complete(request, call_headers).await?;
        response.usage.duration_ms = Some(elapsed_ms(started));
        Ok(response)
    }

    /// [`Self::stream`] with per-call wire headers (TD-0022 D1).
    pub async fn stream_with(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatEventStream, ProviderError> {
        let started = std::time::Instant::now();
        let inner = self.inner.stream(request, call_headers).await?;
        Ok(stamp_stream_latency(inner, started))
    }
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Rewrite the terminal `Usage` event with wire-truth latency (W3b): duration
/// spans request dispatch to the usage emission; time-to-first-token is the
/// first delivered event (parity with the metering decorator's TTFT
/// semantics). All other events pass through untouched.
fn stamp_stream_latency(inner: ChatEventStream, started: std::time::Instant) -> ChatEventStream {
    use futures_util::StreamExt;

    Box::pin(async_stream::try_stream! {
        let mut inner = inner;
        let mut ttft_ms: Option<u64> = None;
        while let Some(event) = inner.next().await {
            let event = event?;
            if ttft_ms.is_none() {
                ttft_ms = Some(elapsed_ms(started));
            }
            match event {
                ChatStreamEventV1::Usage { mut usage } => {
                    usage.duration_ms = Some(elapsed_ms(started));
                    usage.time_to_first_token_ms = ttft_ms;
                    yield ChatStreamEventV1::Usage { usage };
                }
                other => yield other,
            }
        }
    })
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
                    .with_auth_scheme(config.anthropic_auth_scheme)
                    .with_headers(config.headers.clone()),
            ),
            ProviderFamily::Cohere => Arc::new(
                Cohere::new(config.base_url.clone(), config.api_key.clone())
                    .with_headers(config.headers.clone()),
            ),
            ProviderFamily::Gemini => Arc::new(
                Gemini::new(config.base_url.clone(), config.api_key.clone())
                    .with_auth_scheme(config.gemini_auth_scheme)
                    .with_headers(config.headers.clone()),
            ),
            ProviderFamily::Ollama => {
                let provider =
                    Ollama::new(config.base_url.clone()).with_headers(config.headers.clone());
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
        // Inference POST replay is opt-in. `None` must not inherit a retrying decorator default:
        // an upstream may have accepted and billed a request before the transport timed out.
        let resilient = ResilientProvider::new(bare).with_retry(
            config.max_retries.unwrap_or(0),
            std::time::Duration::from_millis(200),
        );
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
        headers: http::HeaderMap,
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
        let raw_forwarder = Some(build_raw_forwarder(&config));
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(TypedOpenAiCompat { slug, raw }),
            family: ProviderFamily::OpenAiCompat,
            raw: raw_forwarder,
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
        headers: http::HeaderMap,
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
        let raw_forwarder = Some(build_raw_forwarder(&config));
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::openai_responses_typed::TypedOpenAiResponses::new(
                slug,
                raw,
                OpenAiResponsesProfile::Standard,
            )),
            family: ProviderFamily::OpenAiResponses,
            raw: raw_forwarder,
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
        headers: http::HeaderMap,
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
        let raw_forwarder = Some(build_raw_forwarder(&config));
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::openai_responses_typed::TypedOpenAiResponses::new(
                slug,
                raw,
                OpenAiResponsesProfile::ChatGptCodex,
            )),
            family: ProviderFamily::OpenAiResponses,
            raw: raw_forwarder,
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
        let raw_forwarder = Some(build_raw_forwarder(&config));
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::anthropic_typed::TypedAnthropic::new(raw)),
            family: ProviderFamily::Anthropic,
            raw: raw_forwarder,
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
        let raw_forwarder = Some(build_raw_forwarder(&config));
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::ollama_typed::TypedOllama::new(raw)),
            family: ProviderFamily::Ollama,
            raw: raw_forwarder,
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
        let raw_forwarder = Some(build_raw_forwarder(&config));
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::gemini_typed::TypedGemini::new(raw)),
            family: ProviderFamily::Gemini,
            raw: raw_forwarder,
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
        let raw_forwarder = Some(build_raw_forwarder(&config));
        let raw = self.transport(config);
        ProviderHandle {
            inner: Arc::new(crate::cohere_typed::TypedCohere::new(raw)),
            family: ProviderFamily::Cohere,
            raw: raw_forwarder,
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
        headers: http::HeaderMap,
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

    async fn complete(
        &self,
        mut request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatResponseV1, ProviderError> {
        self.apply_constraints(&mut request)?;
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let req = provider_request(&request, encode_openai_request(&request)?, call_headers);
        let response = self.raw.complete(req).await?;
        let mut decoded = decode_openai_response(response.body, response.usage, &request.model)?;
        if !request.include_native_response {
            // G8: the native body is debug metadata, not contract. Decoded
            // extensions (e.g. "reasoning") always survive.
            decoded.extensions.remove("openai");
        }
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        Ok(decoded)
    }

    async fn stream(
        &self,
        mut request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatEventStream, ProviderError> {
        self.apply_constraints(&mut request)?;
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let req = provider_request(&request, encode_openai_request(&request)?, call_headers);
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

pub(crate) fn provider_request(
    request: &ChatRequestV1,
    body: Value,
    call_headers: http::HeaderMap,
) -> ProviderRequest {
    ProviderRequest::new(request.model.clone(), body)
        .with_session(request.metadata.session_id.clone())
        .with_extra_headers(call_headers)
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
    // W3d/G7: typed fields override any extensions-carried duplicate (inserted
    // after the extensions clone). OpenAI chat / ZAI take reasoning_effort
    // top-level and thinking as `{type, budget_tokens}`.
    insert_optional(
        &mut body,
        "reasoning_effort",
        request.reasoning_effort.clone(),
    );
    if let Some(thinking) = &request.thinking {
        body.insert("thinking".into(), encode_openai_thinking(thinking));
    }
    Ok(Value::Object(body))
}

/// OpenAI-compat / ZAI extended-thinking shape: `{type: enabled|disabled,
/// budget_tokens?}`.
pub(crate) fn encode_openai_thinking(thinking: &sandhi_core::chat::ThinkingV1) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "type".into(),
        Value::String(
            if thinking.enabled {
                "enabled"
            } else {
                "disabled"
            }
            .into(),
        ),
    );
    if let Some(budget) = thinking.budget_tokens {
        obj.insert("budget_tokens".into(), json!(budget));
    }
    Value::Object(obj)
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
        // TD-0014 P1: the shared bounded splitter. One ceiling across both planes; only the
        // over-budget POLICY differs, and it is applied below. See MAX_STREAM_LINE_BYTES.
        let mut splitter = crate::linesplit::LineSplitter::new(crate::MAX_STREAM_LINE_BYTES);
        let mut started = false;
        let mut open_tools = BTreeMap::<u32, ()>::new();
        let mut emitted_usage = false;
        // TD-0014 P2b: after the real chunks end, ONE synthetic empty chunk flushes any
        // trailing remainder (a final frame without its newline — Ollama's `done` frame) through
        // this same loop body, so per-chunk event ordering is preserved exactly and the body is
        // not duplicated. `raw.next()` returning None terminates; a false flush breaks out.
        let mut tail_pending = false;
        let mut chunks_ended = false;
        while !chunks_ended || tail_pending {
            let chunk = if chunks_ended {
                tail_pending = false;
                crate::StreamChunk {
                    data: bytes::Bytes::new(),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }
            } else {
                match raw.next().await {
                    Some(chunk) => chunk?,
                    None => {
                        chunks_ended = true;
                        tail_pending = splitter.flush_newline();
                        continue;
                    }
                }
            };
            let attempts = chunk.attempts;
            // Unconditional: the synthetic tail chunk arrives with empty data and must still
            // drain the flushed remainder; draining after no new bytes is a no-op scan.
            // (The terminal usage-only chunk is likewise empty and previously skipped this.)
            {
                splitter.push(&chunk.data);
                while let Some(line) = splitter.next_line() {
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
                // TD-0014 P1 (gap G01): past MAX_STREAM_LINE_BYTES the upstream has sent no
                // line boundary at all, which no real provider does. The raw plane drops the
                // pending line and keeps streaming — its bytes were already forwarded, so
                // only usage suffers. A typed decoder emits decoded CONTENT, so dropping
                // silently would corrupt the response with no signal. Fail loudly instead;
                // mid-stream errors are never retried.
                if splitter.over_budget() {
                    Err(ProviderError::Transport(format!(
                        "upstream stream exceeded {} bytes with no line boundary",
                        crate::MAX_STREAM_LINE_BYTES
                    )))?;
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
        let mut details = BTreeMap::new();
        let mut request_id = None;
        let (code, retryable, http_status) = match self {
            Self::InvalidRequest(_) => ("invalid_request", false, Some(400)),
            Self::Auth => ("authentication_error", false, Some(401)),
            Self::RateLimited => ("rate_limited", true, Some(429)),
            Self::Upstream {
                status,
                body,
                request_id: upstream_request_id,
            } => {
                if let Some(body) = body {
                    details.insert("upstream_body".to_owned(), Value::String(body.clone()));
                }
                request_id = upstream_request_id.clone();
                ("upstream_error", *status >= 500, Some(*status))
            }
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
            request_id,
            details,
        }
    }
}

#[cfg(test)]
mod tests {

    /// TD-0014 P1, the opposing guard. The six `..._is_bounded_and_errors_...` tests pin the
    /// ceiling from BELOW; on their own they would all still pass with the bound set to 1 KiB,
    /// which would break every legitimate large frame. Adversarial review found five of six
    /// decoders had no test in this direction — the same one-sidedness that let the original
    /// 64 KiB ceiling ship. This pins it from above.
    #[tokio::test]
    async fn a_large_but_legitimate_frame_is_not_killed_by_the_line_bound() {
        use futures_util::StreamExt;
        // 200 KB in one terminated line: far past the old 64 KiB bound, far under the real one.
        let big = "x".repeat(200 * 1024);
        let wire = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{big}\"}}}}]}}\n\n");
        let chunks: Vec<_> = wire
            .as_bytes()
            .chunks(16 * 1024)
            .map(|c| {
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::copy_from_slice(c),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                })
            })
            .collect();
        let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(chunks));
        let results: Vec<_> = super::decode_openai_stream(raw, "m".into())
            .collect::<Vec<_>>()
            .await;
        assert!(
            !results
                .iter()
                .any(|r| matches!(r, Err(crate::ProviderError::Transport(_)))),
            "a 200 KB terminated frame is legitimate traffic and must survive"
        );
        let text: usize = results
            .iter()
            .flatten()
            .filter_map(|e| match e {
                sandhi_core::ChatStreamEventV1::TextDelta { delta } => Some(delta.len()),
                _ => None,
            })
            .sum();
        assert_eq!(text, big.len(), "the whole delta must arrive intact");
    }

    /// TD-0014 P1 (gap G01): a newline-free upstream stream must stay BOUNDED and fail loudly.
    ///
    /// The raw plane may drop an over-budget line and keep going — its bytes were already
    /// forwarded verbatim, so only *usage* is lost. A typed decoder drops decoded **content**,
    /// so dropping silently would corrupt the response with no signal at all. It errors instead.
    /// Mid-stream errors are never retried (`resilience.rs`), so this cannot loop.
    #[tokio::test]
    async fn a_newline_free_stream_is_bounded_and_errors_rather_than_growing() {
        use futures_util::StreamExt;
        // 16 MiB with no line boundary anywhere — past MAX_STREAM_LINE_BYTES (8 MiB). The bound
        // exists to stop unbounded growth, so the test input has to be genuinely pathological;
        // a merely LARGE frame is legitimate and is covered by the regression test above.
        let filler = bytes::Bytes::from(vec![b'x'; 64 * 1024]);
        let chunks: Vec<_> = (0..256)
            .map(|_| {
                Ok(crate::StreamChunk {
                    data: filler.clone(),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                })
            })
            .collect();
        let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(chunks));
        let results: Vec<_> = super::decode_openai_stream(raw, "m".into())
            .collect::<Vec<_>>()
            .await;
        assert!(
            results
                .iter()
                .any(|item| matches!(item, Err(crate::ProviderError::Transport(_)))),
            "a newline-free stream must terminate with a Transport error rather than \
             buffering without bound"
        );
    }
    use super::*;
    use crate::{ProviderRequest, ProviderResponse};
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

    // W3d/G7: promoted typed fields (reasoning_effort, thinking).
    fn w3d_request(extra: Value) -> ChatRequestV1 {
        let mut base = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn openai_chat_encodes_reasoning_effort_and_thinking() {
        let body = encode_openai_request(&w3d_request(json!({
            "reasoning_effort": "high",
            "thinking": {"enabled": true, "budget_tokens": 2048}
        })))
        .unwrap();
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn openai_chat_disabled_thinking_encodes_disabled() {
        let body = encode_openai_request(&w3d_request(json!({
            "thinking": {"enabled": false}
        })))
        .unwrap();
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn typed_reasoning_effort_overrides_extensions_duplicate() {
        // Precedence: the typed field wins over a stale extensions copy.
        let body = encode_openai_request(&w3d_request(json!({
            "reasoning_effort": "high",
            "extensions": {"openai": {"reasoning_effort": "low"}}
        })))
        .unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn absent_w3d_fields_stay_off_the_wire() {
        let body = encode_openai_request(&w3d_request(json!({}))).unwrap();
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
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
                    usage_running: None,
                    attempts: 3,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse[split..]),
                    usage: None,
                    usage_running: None,
                    attempts: 3,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::new(),
                    usage: Some(ParsedUsage {
                        tokens_in: 6,
                        tokens_out: 5,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 4,
                        reasoning_tokens: 0,
                    }),
                    usage_running: None,
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

    #[test]
    fn factory_handles_carry_a_raw_forwarder_escape_hatch_does_not() {
        // TD-0006: a config-built handle exposes the same-family raw forwarder for the transparent
        // plane; a host-owned escape-hatch handle (no transport config) has none → typed fallback.
        let runtime = ProviderRuntime::new();
        let anthropic = runtime.anthropic(
            "https://api.anthropic.com",
            "k",
            crate::AnthropicAuthScheme::ApiKey,
            None,
            None,
            None,
        );
        assert!(
            anthropic.raw_forwarder().is_some(),
            "a factory handle carries a raw forwarder for the transparent plane"
        );
        let escape_hatch = ProviderHandle::new(Arc::new(NoOpProvider));
        assert!(
            escape_hatch.raw_forwarder().is_none(),
            "an escape-hatch handle has no transport config to forward with"
        );
    }

    /// TD-0022 D1 end to end: per-call wire headers entered at the handle cross the typed
    /// codec and reach the adapter's HTTP request — the FFI seam for turn-scoped gateway
    /// metadata (`x-sandhi-step-id`). The wiremock header matcher is the assertion.
    #[tokio::test]
    async fn complete_with_threads_per_call_headers_to_the_adapter() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("x-sandhi-step-id", "step-7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "r1", "model": "m",
                "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1}
            })))
            .mount(&server)
            .await;

        let handle = ProviderRuntime::new().openai_compat(
            "openai",
            server.uri(),
            "k",
            Default::default(),
            Some(0),
            None,
            None,
        );
        let request: ChatRequestV1 = serde_json::from_value(serde_json::json!({
            "schema_version": "1",
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let mut call_headers = http::HeaderMap::new();
        call_headers.insert(
            http::HeaderName::from_static("x-sandhi-step-id"),
            http::HeaderValue::from_static("step-7"),
        );
        let response = handle.complete_with(request, call_headers).await.unwrap();
        assert_eq!(response.usage.tokens_in, 3);
    }

    /// Canned raw provider for complete()-level tests of the native-body gate (G8).
    struct CannedRaw(Value);
    #[async_trait]
    impl Provider for CannedRaw {
        fn slug(&self) -> &str {
            "openai"
        }
        async fn complete(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            Ok(ProviderResponse {
                status: 200,
                body: self.0.clone(),
                usage: ParsedUsage::default(),
                attempts: 1,
            })
        }
        async fn stream(&self, _: ProviderRequest) -> Result<ByteStream, ProviderError> {
            unreachable!()
        }
    }

    fn canned_openai_provider() -> TypedOpenAiCompat {
        TypedOpenAiCompat {
            slug: "openai".into(),
            raw: Arc::new(CannedRaw(json!({
                "id": "r1",
                "model": "gpt-test",
                "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))),
        }
    }

    #[tokio::test]
    async fn native_body_is_included_by_default() {
        let out = canned_openai_provider()
            .complete(request(), http::HeaderMap::new())
            .await
            .unwrap();
        assert!(out.extensions.contains_key("openai"));
    }

    #[tokio::test]
    async fn include_native_response_false_strips_the_native_body() {
        let mut req = request();
        req.include_native_response = false;
        let out = canned_openai_provider()
            .complete(req, http::HeaderMap::new())
            .await
            .unwrap();
        assert!(!out.extensions.contains_key("openai"));
        // Neutral contract untouched.
        assert_eq!(out.output.content, Some(MessageContent::Text("hi".into())));
        assert_eq!(out.finish_reason, Some(FinishReasonV1::Stop));
    }

    /// Canned typed provider for the W3b latency-stamp tests.
    struct CannedChat;
    #[async_trait]
    impl ChatProvider for CannedChat {
        fn slug(&self) -> &str {
            "canned"
        }
        async fn complete(
            &self,
            _: ChatRequestV1,
            _: http::HeaderMap,
        ) -> Result<ChatResponseV1, ProviderError> {
            Ok(serde_json::from_value(json!({
                "schema_version": "1",
                "model": "m",
                "output": {"content": "hi"},
                "usage": {"tokens_in": 1, "tokens_out": 1,
                          "cache_creation_tokens": 0, "cache_read_tokens": 0}
            }))
            .unwrap())
        }
        async fn stream(
            &self,
            _: ChatRequestV1,
            _: http::HeaderMap,
        ) -> Result<ChatEventStream, ProviderError> {
            let events: Vec<Result<ChatStreamEventV1, ProviderError>> = vec![
                Ok(ChatStreamEventV1::TextDelta { delta: "hi".into() }),
                Ok(ChatStreamEventV1::Usage {
                    usage: serde_json::from_value(json!({
                        "tokens_in": 1, "tokens_out": 1,
                        "cache_creation_tokens": 0, "cache_read_tokens": 0
                    }))
                    .unwrap(),
                }),
            ];
            Ok(Box::pin(futures_util::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn handle_complete_stamps_wire_duration() {
        let handle = ProviderHandle::new(Arc::new(CannedChat));
        let out = handle
            .complete(
                serde_json::from_value(json!({
                    "model": "m",
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(out.usage.duration_ms.is_some());
        assert!(out.usage.time_to_first_token_ms.is_none()); // streaming-only
    }

    #[tokio::test]
    async fn handle_stream_stamps_latency_on_terminal_usage() {
        let handle = ProviderHandle::new(Arc::new(CannedChat));
        let mut stream = handle
            .stream(
                serde_json::from_value(json!({
                    "model": "m",
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            if let ChatStreamEventV1::Usage { usage: u } = event.unwrap() {
                usage = Some(u);
            }
        }
        let usage = usage.expect("terminal usage event");
        assert!(usage.duration_ms.is_some());
        assert!(usage.time_to_first_token_ms.is_some());
        assert!(usage.time_to_first_token_ms <= usage.duration_ms);
    }

    /// Minimal ChatProvider mock for handle tests (never actually completes a call).
    struct NoOpProvider;
    #[async_trait]
    impl ChatProvider for NoOpProvider {
        fn slug(&self) -> &str {
            "noop"
        }
        async fn complete(
            &self,
            _: ChatRequestV1,
            _: http::HeaderMap,
        ) -> Result<ChatResponseV1, ProviderError> {
            unreachable!()
        }
        async fn stream(
            &self,
            _: ChatRequestV1,
            _: http::HeaderMap,
        ) -> Result<ChatEventStream, ProviderError> {
            unreachable!()
        }
    }

    #[test]
    fn upstream_error_body_reaches_typed_details_and_message() {
        let body = r#"{"error":{"message":"tool call id call_9 not found in messages"}}"#;
        let err = ProviderError::Upstream {
            status: 400,
            body: Some(body.to_owned()),
            request_id: None,
        };
        let typed = err.as_typed(Some("moonshot"));
        assert_eq!(typed.code, "upstream_error");
        assert_eq!(typed.http_status, Some(400));
        assert!(!typed.retryable);
        assert_eq!(
            typed.details.get("upstream_body"),
            Some(&Value::String(body.to_owned()))
        );
        // Display carries a single-line snippet so consumer logs are self-explaining.
        assert!(typed.message.contains("tool call id call_9"));
    }

    #[test]
    fn upstream_error_without_body_keeps_prior_shape() {
        let err = ProviderError::Upstream {
            status: 502,
            body: None,
            request_id: None,
        };
        let typed = err.as_typed(None);
        assert_eq!(typed.message, "upstream status 502");
        assert!(typed.retryable);
        assert!(typed.details.is_empty());
    }

    #[test]
    fn upstream_request_id_reaches_typed_error_and_display() {
        let err = ProviderError::Upstream {
            status: 400,
            body: Some(r#"{"error":"bad tool pairing"}"#.to_owned()),
            request_id: Some("req_abc123".to_owned()),
        };
        let typed = err.as_typed(Some("moonshot"));
        assert_eq!(typed.request_id.as_deref(), Some("req_abc123"));
        assert!(err.to_string().contains("[request-id: req_abc123]"));
    }

    #[test]
    fn upstream_without_request_id_stays_none() {
        let err = ProviderError::Upstream {
            status: 502,
            body: None,
            request_id: None,
        };
        let typed = err.as_typed(None);
        assert!(typed.request_id.is_none());
        assert_eq!(err.to_string(), "upstream status 502");
    }

    #[test]
    fn display_snippet_is_bounded_and_single_line() {
        let long = format!("line1\nline2 {}", "x".repeat(500));
        let err = ProviderError::Upstream {
            status: 422,
            body: Some(long),
            request_id: None,
        };
        let msg = err.to_string();
        assert!(
            msg.len() < 260,
            "display snippet must stay bounded: {}",
            msg.len()
        );
        assert!(!msg.contains('\n'));
    }

    /// Minimal local import so the test compiles without adding a top-level `use`.
    use reqwest::header::HeaderMap;
}
