//! Sandhi Node binding (napi-rs), published as `@anvailabs/sandhi`.
//!
//! `ProviderRuntime` exposes persistent typed chat-contract handles; provider-native request and
//! response JSON never crosses the binding. The same module exposes metering and budget APIs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use napi::bindgen_prelude::*;
use napi_derive::napi;

use sandhi_core::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_responses_usage, parse_openai_usage, Backend, Budget,
    BudgetLedger, Dimension, KeyStore, ParsedUsage, UsageAggregateV1, UsageAggregator, UsageEvent,
    VirtualKey,
};
use sandhi_providers::{
    AnthropicAuthScheme, GeminiAuthScheme, ProviderError, ProviderFamily, ProviderHandle,
    ProviderRuntime as RustProviderRuntime,
};

fn parse_anthropic_auth_scheme(value: Option<&str>) -> Result<AnthropicAuthScheme> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("api_key") => Ok(AnthropicAuthScheme::ApiKey),
        Some("bearer") => Ok(AnthropicAuthScheme::Bearer),
        Some(other) => Err(Error::from_reason(format!(
            "unsupported Anthropic auth_scheme {other:?}; expected 'api_key' or 'bearer'"
        ))),
    }
}

fn parse_gemini_auth_scheme(value: Option<&str>) -> Result<GeminiAuthScheme> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("api_key") => Ok(GeminiAuthScheme::ApiKey),
        Some("bearer") => Ok(GeminiAuthScheme::Bearer),
        Some(other) => Err(Error::from_reason(format!(
            "unsupported Gemini auth_scheme {other:?}; expected 'api_key' or 'bearer'"
        ))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenAiProtocol {
    ChatCompletions,
    Responses,
    ChatGptResponses,
}

fn parse_openai_protocol(value: Option<&str>) -> Result<OpenAiProtocol> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("chat_completions") | Some("openai_chat_completions") => {
            Ok(OpenAiProtocol::ChatCompletions)
        }
        Some("responses") | Some("openai_responses") => Ok(OpenAiProtocol::Responses),
        Some("chatgpt_responses") | Some("codex_responses") => {
            Ok(OpenAiProtocol::ChatGptResponses)
        }
        Some(other) => Err(Error::from_reason(format!(
            "unsupported protocol {other:?}; expected 'chat_completions', 'responses', or 'chatgpt_responses'"
        ))),
    }
}

fn parse_chat_request(request_json: &str) -> Result<sandhi_core::ChatRequestV1> {
    let request: sandhi_core::ChatRequestV1 = serde_json::from_str(request_json)
        .map_err(|e| Error::from_reason(format!("invalid ChatRequestV1 JSON: {e}")))?;
    request
        .validate()
        .map_err(|e| Error::from_reason(format!("invalid ChatRequestV1: {e}")))?;
    Ok(request)
}

fn typed_provider_error(error: ProviderError, provider: &str) -> Error {
    let typed = error.as_typed(Some(provider));
    Error::from_reason(serde_json::to_string(&typed).unwrap_or_else(|_| error.to_string()))
}

/// Persistent factory for typed provider handles. The HTTP pool, retry policy, and circuit
/// breaker belong to each returned handle rather than being rebuilt for every request.
#[napi(js_name = "ProviderRuntime")]
pub struct JsProviderRuntime {
    inner: RustProviderRuntime,
}

impl Default for JsProviderRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[napi]
impl JsProviderRuntime {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            inner: RustProviderRuntime::new(),
        }
    }

    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn openai_compat(
        &self,
        provider: String,
        base_url: String,
        api_key: String,
        headers_json: Option<String>,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> Result<TypedProvider> {
        let headers = parse_headers_json(headers_json)?;
        let handle = self.inner.openai_compat(
            provider.clone(),
            base_url,
            api_key,
            headers,
            max_retries,
            timeout_secs,
            stream_idle_timeout_secs,
        );
        Ok(TypedProvider { provider, handle })
    }

    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn openai_responses(
        &self,
        provider: String,
        base_url: String,
        bearer_token: String,
        headers_json: Option<String>,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
    ) -> Result<TypedProvider> {
        let handle = self.inner.openai_responses(
            provider.clone(),
            base_url,
            bearer_token,
            parse_headers_json(headers_json)?,
            max_retries,
            timeout_secs,
            stream_idle_timeout_secs,
        );
        Ok(TypedProvider { provider, handle })
    }

    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn provider(
        &self,
        provider: String,
        model: String,
        api_key: String,
        base_url: Option<String>,
        headers_json: Option<String>,
        max_retries: Option<u32>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
        auth_scheme: Option<String>,
        protocol: Option<String>,
    ) -> Result<TypedProvider> {
        let normalized = provider.trim().to_ascii_lowercase();
        let protocol = parse_openai_protocol(protocol.as_deref())?;
        // Contract principle (TD-0008): `authScheme: "bearer"` is a no-op for
        // families whose default IS Bearer — accepted family-agnostically for
        // gateway callers; only a contradictory scheme is rejected. Parity with
        // the Python binding.
        if let Some(scheme) = auth_scheme
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let scheme_families = matches!(
                normalized.as_str(),
                "anthropic" | "claude" | "gemini" | "google"
            );
            if !scheme_families && scheme != "bearer" {
                return Err(Error::from_reason(format!(
                    "authScheme {scheme:?} is only valid for the Anthropic or Gemini \
protocol; other families always authenticate with 'Authorization: Bearer' \
(authScheme=\"bearer\" is accepted there as a no-op)"
                )));
            }
        }
        let handle = if protocol != OpenAiProtocol::ChatCompletions {
            let resolved_base_url = if let Some(base_url) = base_url {
                base_url
            } else {
                sandhi_providers::resolve_openai_compat_provider(&provider)
                    .map(|spec| spec.base_url_for_model(&model).to_owned())
                    .ok_or_else(|| {
                        Error::from_reason(
                            "Responses protocol requires baseUrl for an unknown provider",
                        )
                    })?
            };
            let headers = parse_headers_json(headers_json)?;
            if protocol == OpenAiProtocol::ChatGptResponses {
                self.inner.chatgpt_responses(
                    provider.clone(),
                    resolved_base_url,
                    api_key,
                    headers,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                )
            } else {
                self.inner.openai_responses(
                    provider.clone(),
                    resolved_base_url,
                    api_key,
                    headers,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                )
            }
        } else {
            match normalized.as_str() {
                "anthropic" | "claude" => self.inner.anthropic(
                    base_url.unwrap_or_else(|| ProviderFamily::Anthropic.default_base_url().into()),
                    api_key,
                    parse_anthropic_auth_scheme(auth_scheme.as_deref())?,
                    parse_headers_json(headers_json.clone())?,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                ),
                "gemini" | "google" => self.inner.gemini(
                    base_url.unwrap_or_else(|| ProviderFamily::Gemini.default_base_url().into()),
                    api_key,
                    parse_gemini_auth_scheme(auth_scheme.as_deref())?,
                    parse_headers_json(headers_json.clone())?,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                ),
                "cohere" => self.inner.cohere(
                    base_url.unwrap_or_else(|| ProviderFamily::Cohere.default_base_url().into()),
                    api_key,
                    parse_headers_json(headers_json.clone())?,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                ),
                "ollama" => self.inner.ollama(
                    base_url.unwrap_or_else(|| ProviderFamily::Ollama.default_base_url().into()),
                    api_key,
                    parse_headers_json(headers_json.clone())?,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                ),
                _ => {
                    let headers = parse_headers_json(headers_json)?;
                    if let Some(base_url) = base_url {
                        self.inner.openai_compat(
                            normalized,
                            base_url,
                            api_key,
                            headers,
                            max_retries,
                            timeout_secs,
                            stream_idle_timeout_secs,
                        )
                    } else {
                        self.inner
                            .known_openai_compat(
                                &normalized,
                                &model,
                                api_key,
                                headers,
                                max_retries,
                                timeout_secs,
                                stream_idle_timeout_secs,
                            )
                            .map_err(|error| typed_provider_error(error, &normalized))?
                    }
                }
            }
        };
        Ok(TypedProvider {
            provider: handle.slug().to_owned(),
            handle,
        })
    }
}

fn parse_headers_json(value: Option<String>) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    let Some(value) = value else {
        return Ok(headers);
    };
    let values: HashMap<String, String> = serde_json::from_str(&value)
        .map_err(|e| Error::from_reason(format!("headersJson is not a string map: {e}")))?;
    for (name, value) in values {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| Error::from_reason(format!("invalid header name: {e}")))?;
        let value = reqwest::header::HeaderValue::from_str(&value)
            .map_err(|e| Error::from_reason(format!("invalid header value: {e}")))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

/// A persistent provider handle accepting Sandhi chat-contract v1 JSON documents.
#[napi]
pub struct TypedProvider {
    provider: String,
    handle: ProviderHandle,
}

#[napi]
impl TypedProvider {
    #[napi(getter)]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// `wireHeadersJson` (optional, TD-0022 D1): per-call wire headers — a string map sent
    /// with THIS call only, for gateway-path metadata that changes per turn (e.g.
    /// `x-sandhi-step-id`). Transport-owned names are stripped and can never override the
    /// credential.
    #[napi]
    pub async fn complete_json(
        &self,
        request_json: String,
        wire_headers_json: Option<String>,
    ) -> Result<String> {
        let call_headers = parse_headers_json(wire_headers_json)?;
        let request = parse_chat_request(&request_json)?;
        let response = self
            .handle
            .complete_with(request, call_headers)
            .await
            .map_err(|e| typed_provider_error(e, &self.provider))?;
        serde_json::to_string(&response).map_err(|e| Error::from_reason(e.to_string()))
    }

    /// `wireHeadersJson` (optional, TD-0022 D1): per-call wire headers, as `completeJson`.
    #[napi]
    pub fn stream_json(
        &self,
        request_json: String,
        wire_headers_json: Option<String>,
    ) -> Result<TypedEventStream> {
        let call_headers = parse_headers_json(wire_headers_json)?;
        let request = parse_chat_request(&request_json)?;
        let handle = self.handle.clone();
        let provider = self.provider.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<String, String>>(64);
        tokio::spawn(async move {
            use futures_util::StreamExt;
            match handle.stream_with(request, call_headers).await {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        let (item, stop) = match event {
                            Ok(event) => (
                                serde_json::to_string(&event).map_err(|e| e.to_string()),
                                false,
                            ),
                            Err(error) => (
                                Err(serde_json::to_string(&error.as_typed(Some(&provider)))
                                    .unwrap_or_else(|_| error.to_string())),
                                true,
                            ),
                        };
                        if tx.send(item).await.is_err() || stop {
                            break;
                        }
                    }
                }
                Err(error) => {
                    let _ = tx
                        .send(Err(serde_json::to_string(&error.as_typed(Some(&provider)))
                            .unwrap_or_else(|_| error.to_string())))
                        .await;
                }
            }
        });
        Ok(TypedEventStream {
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
        })
    }
}

/// Pull-based stream of serialized `ChatStreamEventV1` documents.
#[napi]
pub struct TypedEventStream {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<std::result::Result<String, String>>>>,
}

#[napi]
impl TypedEventStream {
    #[napi]
    pub async fn read(&self) -> Result<Option<String>> {
        match self.rx.lock().await.recv().await {
            Some(Ok(item)) => Ok(Some(item)),
            Some(Err(error)) => Err(Error::from_reason(error)),
            None => Ok(None),
        }
    }
}

/// The neutral token breakdown parsed from a provider response.
#[napi(object)]
pub struct UsageBreakdown {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub cache_creation_tokens: u32,
    pub cache_read_tokens: u32,
}

/// A neutral usage event (mirrors `usage-event.v1.schema.json`).
#[napi(object)]
pub struct Event {
    pub schema_version: String,
    pub request_id: String,
    pub occurred_at: String,
    pub provider: String,
    pub model: String,
    pub backend: String,
    pub virtual_key_id: Option<String>,
    pub subject_id: Option<String>,
    pub group_id: Option<String>,
    pub route: Option<String>,
    pub session_id: Option<String>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub cache_creation_tokens: u32,
    pub cache_read_tokens: u32,
    pub usage_completeness: String,
    pub attempts: u32,
    pub outcome: Option<String>,
    pub upstream_request_id: Option<String>,
    pub gpu_seconds: Option<f64>,
}

/// The usage-event wire-contract major version this build targets.
#[napi]
pub fn wire_contract_version() -> String {
    UsageEvent::SCHEMA_VERSION.to_string()
}

/// The neutral chat-contract (ChatRequestV1/ChatStreamEventV1) major version this
/// build targets. Today equal to `wire_contract_version()` (pinned by a
/// sandhi-core test); exported separately so a future split cannot silently
/// invalidate consumer handshakes that validate the chat contract.
#[napi]
pub fn chat_contract_version() -> String {
    sandhi_core::chat::CHAT_SCHEMA_VERSION_V1.to_string()
}

/// Additive growth counter within the v1 chat contract (W3c). Consumers
/// feature-detect this export — an older binding without it reads as minor 0
/// — and gate trust in newer additive fields on a minimum minor.
#[napi]
pub fn chat_contract_minor() -> u32 {
    sandhi_core::chat::CHAT_CONTRACT_MINOR
}

/// Resolve an OpenAI-compatible provider spec (slug, aliases, base_url) as JSON.
/// Parity with the Python binding's `provider_spec` (TD-0008 P4); JSON here
/// because napi objects would otherwise diverge from the schema'd facades.
#[napi]
pub fn provider_spec_json(provider: String, model: Option<String>) -> Result<String> {
    let spec = sandhi_providers::resolve_openai_compat_provider(&provider)
        .ok_or_else(|| Error::from_reason(format!("unknown provider: {provider}")))?;
    let base_url = model
        .as_deref()
        .map_or(spec.base_url, |name| spec.base_url_for_model(name));
    serde_json::to_string(&serde_json::json!({
        "slug": spec.slug,
        "aliases": spec.aliases,
        "base_url": base_url,
    }))
    .map_err(|error| Error::from_reason(format!("serialize provider spec: {error}")))
}

/// Return a checked chat-contract JSON Schema document by name
/// (e.g. "chat-stream-event.v1"). Parity with the Python binding.
#[napi]
pub fn chat_contract_schema_json(name: String) -> Result<String> {
    let filename = if name.ends_with(".schema.json") {
        name.clone()
    } else {
        format!("{name}.schema.json")
    };
    sandhi_core::contract_schema_documents()
        .remove(filename.as_str())
        .ok_or_else(|| Error::from_reason(format!("unknown chat contract schema: {name}")))
}

/// Return the versioned typed descriptor for a known provider as JSON.
#[napi]
pub fn provider_descriptor_json(provider: String) -> Result<String> {
    let descriptor = sandhi_providers::provider_descriptor(&provider)
        .ok_or_else(|| Error::from_reason(format!("unknown provider: {provider}")))?;
    serde_json::to_string(&descriptor)
        .map_err(|error| Error::from_reason(format!("serialize provider descriptor: {error}")))
}

/// Return the curated model descriptors for a known provider as a JSON list (TD-0004 catalog DATA).
/// Carries facts only (id, context window, max output, capabilities); no pricing.
#[napi]
pub fn provider_models_json(provider: String) -> Result<String> {
    let descriptor = sandhi_providers::provider_descriptor(&provider)
        .ok_or_else(|| Error::from_reason(format!("unknown provider: {provider}")))?;
    serde_json::to_string(&descriptor.models)
        .map_err(|error| Error::from_reason(format!("serialize provider models: {error}")))
}

/// Parse a provider response body (JSON string) into the neutral token breakdown. `provider`
/// selects the parser: `anthropic` → the Anthropic Messages shape; anything else → OpenAI-compat.
#[napi]
pub fn parse_usage(provider: String, response_json: String) -> Result<UsageBreakdown> {
    let value: serde_json::Value = serde_json::from_str(&response_json)
        .map_err(|e| Error::from_reason(format!("response_json is not valid JSON: {e}")))?;
    Ok(usage_breakdown(&parse_for(&provider, &value)))
}

/// In-process metering middleware: virtual keys, budgets, and usage-event emission.
#[napi]
pub struct Gateway {
    inner: Mutex<Inner>,
    counter: AtomicU64,
}

struct Inner {
    keys: KeyStore,
    ledger: BudgetLedger,
    events: Vec<UsageEvent>,
    jsonl_path: Option<PathBuf>,
}

#[napi]
impl Gateway {
    /// `sinkPath` — append emitted events as JSONL to this file (plus an in-memory buffer).
    #[napi(constructor)]
    pub fn new(sink_path: Option<String>) -> Self {
        Gateway {
            inner: Mutex::new(Inner {
                keys: KeyStore::new(),
                ledger: BudgetLedger::new(),
                events: Vec::new(),
                jsonl_path: sink_path.map(PathBuf::from),
            }),
            counter: AtomicU64::new(0),
        }
    }

    /// Register a virtual key: `id` → subject/group attribution + an opaque `upstream` ref.
    #[napi]
    pub fn add_virtual_key(
        &self,
        id: String,
        subject: Option<String>,
        group: Option<String>,
        upstream: Option<String>,
    ) {
        self.inner.lock().unwrap().keys.insert(VirtualKey {
            id,
            subject_id: subject,
            group_id: group,
            upstream_ref: upstream.unwrap_or_default(),
            ..Default::default()
        });
    }

    /// Set a token budget on a scope (e.g. `group:platform`). Exposed as a 64-bit `bigint` (napi
    /// has no bare `u64`); negatives clamp to 0 — a budget is a non-negative token count.
    #[napi]
    pub fn set_budget(&self, scope: String, tokens: i64) {
        self.inner
            .lock()
            .unwrap()
            .ledger
            .set_limit(scope, Budget::tokens(tokens.max(0) as u64));
    }

    /// Would `add` more tokens be within the scope's budget?
    #[napi]
    pub fn check_budget(&self, scope: String, add: i64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .ledger
            .check(&scope, add.max(0) as u64)
            .is_ok()
    }

    /// Tokens spent so far on a scope.
    #[napi]
    pub fn spent(&self, scope: String) -> i64 {
        i64::try_from(self.inner.lock().unwrap().ledger.spent(&scope)).unwrap_or(i64::MAX)
    }

    /// Meter one completed call: parse usage from `responseJson` (built-in parser for `provider`),
    /// attribute it to `virtualKey`, emit the neutral event + record the budget, and return the
    /// event. Throws on an unknown virtual key or bad JSON.
    #[napi]
    pub fn meter(
        &self,
        virtual_key: String,
        provider: String,
        model: String,
        response_json: String,
        session_id: Option<String>,
        route: Option<String>,
    ) -> Result<Event> {
        let value: serde_json::Value = serde_json::from_str(&response_json)
            .map_err(|e| Error::from_reason(format!("response_json is not valid JSON: {e}")))?;
        let parsed = parse_for(&provider, &value);
        self.record_and_build(&virtual_key, &provider, &model, parsed, session_id, route)
    }

    /// Meter from token counts you supply directly (bypass parsing) — the escape hatch for any
    /// provider Sandhi doesn't natively parse: do your own parsing and pass the counts. Same
    /// attribution + budget + emit as `meter()`.
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub fn meter_tokens(
        &self,
        virtual_key: String,
        provider: String,
        model: String,
        tokens_in: u32,
        tokens_out: u32,
        cache_creation_tokens: Option<u32>,
        cache_read_tokens: Option<u32>,
        session_id: Option<String>,
        route: Option<String>,
    ) -> Result<Event> {
        let parsed = ParsedUsage {
            tokens_in: u64::from(tokens_in),
            tokens_out: u64::from(tokens_out),
            cache_creation_tokens: u64::from(cache_creation_tokens.unwrap_or(0)),
            cache_read_tokens: u64::from(cache_read_tokens.unwrap_or(0)),
            reasoning_tokens: 0,
        };
        self.record_and_build(&virtual_key, &provider, &model, parsed, session_id, route)
    }

    /// All events emitted so far (in-memory).
    #[napi]
    pub fn events(&self) -> Vec<Event> {
        self.inner
            .lock()
            .unwrap()
            .events
            .iter()
            .map(event_to_napi)
            .collect()
    }

    /// Fold the events recorded so far into `UsageAggregateV1` rows for one attribution
    /// `dimension`, serialized as a JSON array (TD-0009 P2). `dimension` is one of `subject`
    /// (`user`), `group`, `provider`, `model`, `key` (`virtual_key`), `session`, or `total`.
    ///
    /// The fold is `sandhi_core::UsageAggregator` — the same definition the proxy, the CLI, and
    /// the dashboard read, so the in-process path cannot disagree with them. `cap` bounds the
    /// number of distinct keys before the rest fold into `"(overflow)"` (default 1024): a
    /// long-lived process metering unbounded `sessionId`s loses per-key detail, never the sum —
    /// the rows always add up to the exact total. Throws on an unknown dimension.
    #[napi]
    pub fn usage_snapshot_json(&self, dimension: String, cap: Option<u32>) -> Result<String> {
        let parsed = Dimension::parse(&dimension).ok_or_else(|| {
            Error::from_reason(format!(
                "unknown usage dimension {dimension:?}; expected one of \
subject, group, provider, model, key, session, total"
            ))
        })?;
        let inner = self.inner.lock().unwrap();
        serde_json::to_string(&fold_usage(
            &inner.events,
            parsed,
            cap.map(|cap| cap as usize),
        ))
        .map_err(|e| Error::from_reason(format!("serialize usage aggregate: {e}")))
    }
}

impl Gateway {
    fn next_request_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("req_{millis}_{n}")
    }

    /// Shared tail: resolve the key, build + emit the event, record the budget, return it.
    fn record_and_build(
        &self,
        virtual_key: &str,
        provider: &str,
        model: &str,
        parsed: ParsedUsage,
        session_id: Option<String>,
        route: Option<String>,
    ) -> Result<Event> {
        let mut inner = self.inner.lock().unwrap();
        let vk = inner
            .keys
            .resolve(virtual_key)
            .ok_or_else(|| Error::from_reason(format!("unknown virtual key: {virtual_key}")))?;

        let event = parsed.apply(
            UsageEvent::new(
                self.next_request_id(),
                now_rfc3339(),
                provider,
                model,
                Backend::External,
            )
            .with_attribution(
                Some(vk.id.clone()),
                vk.subject_id.clone(),
                vk.group_id.clone(),
            )
            .with_session(session_id)
            .with_route(route),
        );

        if let Some(path) = &inner.jsonl_path {
            if let Ok(line) = serde_json::to_string(&event) {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
        let scope = match &vk.group_id {
            Some(g) => format!("group:{g}"),
            None => format!("vk:{}", vk.id),
        };
        inner.ledger.record(&scope, event.billable_tokens());
        inner.events.push(event.clone());
        drop(inner);

        Ok(event_to_napi(&event))
    }
}

/// Fold recorded events into aggregate rows for one dimension. Mirrors the Python binding exactly
/// (same core fold, same insertion order, same serializer), which is what makes the two
/// snapshots byte-identical rather than merely similar (TD-0009 D6).
fn fold_usage(
    events: &[UsageEvent],
    dimension: Dimension,
    cap: Option<usize>,
) -> Vec<UsageAggregateV1> {
    let mut agg = match cap {
        Some(cap) => UsageAggregator::with_cap(dimension, cap),
        None => UsageAggregator::new(dimension),
    };
    for event in events {
        agg.add(event);
    }
    agg.rows()
}

fn parse_for(provider: &str, value: &serde_json::Value) -> ParsedUsage {
    match provider {
        "anthropic" => parse_anthropic_usage(value),
        "gemini" => parse_gemini_usage(value),
        "cohere" => parse_cohere_usage(value),
        "ollama" => parse_ollama_usage(value),
        "bedrock" => parse_bedrock_usage(value),
        "openai_responses" | "responses" => parse_openai_responses_usage(value),
        _ => parse_openai_usage(value),
    }
    .unwrap_or_default()
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn usage_breakdown(u: &ParsedUsage) -> UsageBreakdown {
    UsageBreakdown {
        tokens_in: u.tokens_in as u32,
        tokens_out: u.tokens_out as u32,
        cache_creation_tokens: u.cache_creation_tokens as u32,
        cache_read_tokens: u.cache_read_tokens as u32,
    }
}

fn event_to_napi(e: &UsageEvent) -> Event {
    Event {
        schema_version: e.schema_version.clone(),
        request_id: e.request_id.clone(),
        occurred_at: e.occurred_at.clone(),
        provider: e.provider.clone(),
        model: e.model.clone(),
        backend: match e.backend {
            Backend::External => "external".to_string(),
            Backend::SelfHosted => "self_hosted".to_string(),
        },
        virtual_key_id: e.virtual_key_id.clone(),
        subject_id: e.subject_id.clone(),
        group_id: e.group_id.clone(),
        route: e.route.clone(),
        session_id: e.session_id.clone(),
        tokens_in: e.tokens_in as u32,
        tokens_out: e.tokens_out as u32,
        cache_creation_tokens: e.cache_creation_tokens as u32,
        cache_read_tokens: e.cache_read_tokens as u32,
        usage_completeness: match e.usage_completeness {
            sandhi_core::UsageCompleteness::Final => "final",
            sandhi_core::UsageCompleteness::Partial => "partial",
            sandhi_core::UsageCompleteness::Unavailable => "unavailable",
        }
        .into(),
        attempts: e.attempts,
        outcome: e.outcome.clone(),
        upstream_request_id: e.upstream_request_id.clone(),
        gpu_seconds: e.gpu_seconds,
    }
}
