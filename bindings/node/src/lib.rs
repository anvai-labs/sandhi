//! Sandhi Node binding (napi-rs), published as `@anvai-labs/sandhi`.
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
    BudgetLedger, KeyStore, ParsedUsage, UsageEvent, VirtualKey,
};
use sandhi_providers::{
    AnthropicAuthScheme, ProviderError, ProviderHandle, ProviderRuntime as RustProviderRuntime,
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
        if auth_scheme
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && !matches!(normalized.as_str(), "anthropic" | "claude")
        {
            return Err(Error::from_reason(
                "authScheme is only valid for the Anthropic Messages protocol",
            ));
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
                    base_url.unwrap_or_else(|| "https://api.anthropic.com".into()),
                    api_key,
                    parse_anthropic_auth_scheme(auth_scheme.as_deref())?,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                ),
                "gemini" | "google" => self.inner.gemini(
                    base_url.unwrap_or_else(|| {
                        "https://generativelanguage.googleapis.com/v1beta".into()
                    }),
                    api_key,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                ),
                "cohere" => self.inner.cohere(
                    base_url.unwrap_or_else(|| "https://api.cohere.com".into()),
                    api_key,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                ),
                "ollama" => self.inner.ollama(
                    base_url.unwrap_or_else(|| "http://localhost:11434".into()),
                    api_key,
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

    #[napi]
    pub async fn complete_json(&self, request_json: String) -> Result<String> {
        let request = parse_chat_request(&request_json)?;
        let response = self
            .handle
            .complete(request)
            .await
            .map_err(|e| typed_provider_error(e, &self.provider))?;
        serde_json::to_string(&response).map_err(|e| Error::from_reason(e.to_string()))
    }

    #[napi]
    pub fn stream_json(&self, request_json: String) -> Result<TypedEventStream> {
        let request = parse_chat_request(&request_json)?;
        let handle = self.handle.clone();
        let provider = self.provider.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<String, String>>(64);
        tokio::spawn(async move {
            use futures_util::StreamExt;
            match handle.stream(request).await {
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
        });
    }

    /// Set a token budget on a scope (e.g. `group:platform`).
    #[napi]
    pub fn set_budget(&self, scope: String, tokens: u32) {
        self.inner
            .lock()
            .unwrap()
            .ledger
            .set_limit(scope, Budget::tokens(u64::from(tokens)));
    }

    /// Would `add` more tokens be within the scope's budget?
    #[napi]
    pub fn check_budget(&self, scope: String, add: u32) -> bool {
        self.inner
            .lock()
            .unwrap()
            .ledger
            .check(&scope, u64::from(add))
            .is_ok()
    }

    /// Tokens spent so far on a scope.
    #[napi]
    pub fn spent(&self, scope: String) -> u32 {
        self.inner.lock().unwrap().ledger.spent(&scope) as u32
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
            .cloned()
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
