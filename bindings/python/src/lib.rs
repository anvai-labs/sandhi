//! Sandhi Python binding (PyO3), published as `sandhi-gateway`.
//!
//! `ProviderRuntime` exposes persistent typed chat-contract handles; provider-native request and
//! response JSON never crosses the binding. The same module exposes metering and budget APIs.

// pyo3's #[pyfunction]/#[pymethods] macros emit `.into()` on the PyErr return path; on this
// pyo3 + clippy combo that trips `useless_conversion` inside generated code (not our code).
#![allow(clippy::useless_conversion)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pyo3::exceptions::{PyKeyError, PyRuntimeError, PyStopAsyncIteration, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict};
use std::sync::Arc;

use sandhi_core::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_responses_usage, parse_openai_usage, Backend, Budget,
    BudgetLedger, KeyStore, ParsedUsage, UsageEvent, VirtualKey,
};
use sandhi_providers::{
    resolve_openai_compat_provider, AnthropicAuthScheme, GeminiAuthScheme, ProviderError,
    ProviderHandle, ProviderRuntime as RustProviderRuntime,
};

fn parse_anthropic_auth_scheme(value: Option<&str>) -> PyResult<AnthropicAuthScheme> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("api_key") => Ok(AnthropicAuthScheme::ApiKey),
        Some("bearer") => Ok(AnthropicAuthScheme::Bearer),
        Some(other) => Err(PyValueError::new_err(format!(
            "unsupported Anthropic auth_scheme {other:?}; expected 'api_key' or 'bearer'"
        ))),
    }
}

fn parse_gemini_auth_scheme(value: Option<&str>) -> PyResult<GeminiAuthScheme> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("api_key") => Ok(GeminiAuthScheme::ApiKey),
        Some("bearer") => Ok(GeminiAuthScheme::Bearer),
        Some(other) => Err(PyValueError::new_err(format!(
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

fn parse_openai_protocol(value: Option<&str>) -> PyResult<OpenAiProtocol> {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("chat_completions") | Some("openai_chat_completions") => {
            Ok(OpenAiProtocol::ChatCompletions)
        }
        Some("responses") | Some("openai_responses") => Ok(OpenAiProtocol::Responses),
        Some("chatgpt_responses") | Some("codex_responses") => {
            Ok(OpenAiProtocol::ChatGptResponses)
        }
        Some(other) => Err(PyValueError::new_err(format!(
            "unsupported protocol {other:?}; expected 'chat_completions', 'responses', or 'chatgpt_responses'"
        ))),
    }
}

fn typed_provider_err_to_py(e: ProviderError, provider: &str) -> PyErr {
    let value = e.as_typed(Some(provider));
    let json = serde_json::to_string(&value).unwrap_or_else(|_| e.to_string());
    PyRuntimeError::new_err(json)
}

/// Persistent typed provider factory. Provider handles created by this object retain their Rust
/// HTTP pool and resilience state across calls.
#[pyclass(name = "ProviderRuntime")]
struct PyProviderRuntime {
    inner: RustProviderRuntime,
}

#[pymethods]
impl PyProviderRuntime {
    #[new]
    fn new() -> Self {
        Self {
            inner: RustProviderRuntime::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (provider, base_url, api_key, timeout_secs=None, stream_idle_timeout_secs=None, max_retries=None, headers_json=None))]
    fn openai_compat(
        &self,
        provider: String,
        base_url: String,
        api_key: String,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
        max_retries: Option<u32>,
        headers_json: Option<String>,
    ) -> PyResult<TypedProvider> {
        let handle = self.inner.openai_compat(
            provider.clone(),
            base_url,
            api_key,
            parse_headers_json(headers_json)?,
            max_retries,
            timeout_secs,
            stream_idle_timeout_secs,
        );
        Ok(TypedProvider { provider, handle })
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (provider, base_url, bearer_token, timeout_secs=None, stream_idle_timeout_secs=None, max_retries=None, headers_json=None))]
    fn openai_responses(
        &self,
        provider: String,
        base_url: String,
        bearer_token: String,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
        max_retries: Option<u32>,
        headers_json: Option<String>,
    ) -> PyResult<TypedProvider> {
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

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (provider, model, api_key, base_url=None, timeout_secs=None, stream_idle_timeout_secs=None, max_retries=None, headers_json=None, auth_scheme=None, protocol=None))]
    fn provider(
        &self,
        provider: String,
        model: String,
        api_key: String,
        base_url: Option<String>,
        timeout_secs: Option<f64>,
        stream_idle_timeout_secs: Option<f64>,
        max_retries: Option<u32>,
        headers_json: Option<String>,
        auth_scheme: Option<String>,
        protocol: Option<String>,
    ) -> PyResult<TypedProvider> {
        let normalized = provider.trim().to_ascii_lowercase();
        let protocol = parse_openai_protocol(protocol.as_deref())?;
        if auth_scheme
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && !matches!(normalized.as_str(), "anthropic" | "claude" | "gemini" | "google")
        {
            return Err(PyValueError::new_err(
                "auth_scheme is only valid for the Anthropic or Gemini protocol",
            ));
        }
        let handle = if protocol != OpenAiProtocol::ChatCompletions {
            let resolved_base_url = if let Some(base_url) = base_url {
                base_url
            } else {
                resolve_openai_compat_provider(&provider)
                    .map(|spec| spec.base_url_for_model(&model).to_owned())
                    .ok_or_else(|| {
                        PyValueError::new_err(
                            "Responses protocol requires base_url for an unknown provider",
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
        } else if matches!(normalized.as_str(), "anthropic" | "claude") {
            self.inner.anthropic(
                base_url.unwrap_or_else(|| "https://api.anthropic.com".into()),
                api_key,
                parse_anthropic_auth_scheme(auth_scheme.as_deref())?,
                max_retries,
                timeout_secs,
                stream_idle_timeout_secs,
            )
        } else if matches!(normalized.as_str(), "gemini" | "google") {
            self.inner.gemini(
                base_url
                    .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".into()),
                api_key,
                parse_gemini_auth_scheme(auth_scheme.as_deref())?,
                max_retries,
                timeout_secs,
                stream_idle_timeout_secs,
            )
        } else if normalized == "cohere" {
            self.inner.cohere(
                base_url.unwrap_or_else(|| "https://api.cohere.com".into()),
                api_key,
                max_retries,
                timeout_secs,
                stream_idle_timeout_secs,
            )
        } else if normalized == "ollama" {
            self.inner.ollama(
                base_url.unwrap_or_else(|| "http://localhost:11434".into()),
                api_key,
                max_retries,
                timeout_secs,
                stream_idle_timeout_secs,
            )
        } else if let Some(base_url) = base_url {
            self.inner.openai_compat(
                provider.clone(),
                base_url,
                api_key,
                parse_headers_json(headers_json)?,
                max_retries,
                timeout_secs,
                stream_idle_timeout_secs,
            )
        } else {
            self.inner
                .known_openai_compat(
                    &provider,
                    &model,
                    api_key,
                    parse_headers_json(headers_json)?,
                    max_retries,
                    timeout_secs,
                    stream_idle_timeout_secs,
                )
                .map_err(|error| PyValueError::new_err(error.to_string()))?
        };
        Ok(TypedProvider {
            provider: handle.slug().to_owned(),
            handle,
        })
    }
}

/// A persistent provider handle accepting and returning Sandhi chat-contract v1 JSON documents.
#[pyclass]
struct TypedProvider {
    provider: String,
    handle: ProviderHandle,
}

#[pymethods]
impl TypedProvider {
    #[getter]
    fn provider(&self) -> &str {
        &self.provider
    }

    fn complete_json<'py>(
        &self,
        py: Python<'py>,
        request_json: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request: sandhi_core::ChatRequestV1 = serde_json::from_str(&request_json)
            .map_err(|e| PyValueError::new_err(format!("invalid ChatRequestV1 JSON: {e}")))?;
        request
            .validate()
            .map_err(|e| PyValueError::new_err(format!("invalid ChatRequestV1: {e}")))?;
        let handle = self.handle.clone();
        let provider = self.provider.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = handle
                .complete(request)
                .await
                .map_err(|e| typed_provider_err_to_py(e, &provider))?;
            serde_json::to_string(&response)
                .map_err(|e| PyRuntimeError::new_err(format!("serialize ChatResponseV1: {e}")))
        })
    }

    fn stream_json(&self, request_json: String) -> PyResult<TypedEventStreamIter> {
        let request: sandhi_core::ChatRequestV1 = serde_json::from_str(&request_json)
            .map_err(|e| PyValueError::new_err(format!("invalid ChatRequestV1 JSON: {e}")))?;
        request
            .validate()
            .map_err(|e| PyValueError::new_err(format!("invalid ChatRequestV1: {e}")))?;
        let handle = self.handle.clone();
        let provider = self.provider.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, String>>(64);
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            use futures_util::StreamExt;
            match handle.stream(request).await {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        let (item, stop) = match event {
                            Ok(event) => (
                                serde_json::to_string(&event)
                                    .map_err(|e| format!("serialize ChatStreamEventV1: {e}")),
                                false,
                            ),
                            Err(error) => {
                                let typed = error.as_typed(Some(&provider));
                                (
                                    Err(serde_json::to_string(&typed)
                                        .unwrap_or_else(|_| error.to_string())),
                                    true,
                                )
                            }
                        };
                        if tx.send(item).await.is_err() || stop {
                            break;
                        }
                    }
                }
                Err(error) => {
                    let typed = error.as_typed(Some(&provider));
                    let _ = tx
                        .send(Err(
                            serde_json::to_string(&typed).unwrap_or_else(|_| error.to_string())
                        ))
                        .await;
                }
            }
        });
        Ok(TypedEventStreamIter {
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
        })
    }
}

#[pyclass]
struct TypedEventStreamIter {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Result<String, String>>>>,
}

#[pymethods]
impl TypedEventStreamIter {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.rx.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut guard = rx.lock().await;
            match guard.recv().await {
                Some(Ok(event_json)) => Ok(event_json),
                Some(Err(error_json)) => Err(PyRuntimeError::new_err(error_json)),
                None => Err(PyStopAsyncIteration::new_err(())),
            }
        })
    }
}

fn parse_headers_json(value: Option<String>) -> PyResult<reqwest::header::HeaderMap> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let Some(value) = value else {
        return Ok(HeaderMap::new());
    };
    let entries: HashMap<String, String> = serde_json::from_str(&value)
        .map_err(|e| PyValueError::new_err(format!("headers_json must be a string map: {e}")))?;
    let mut headers = HeaderMap::new();
    for (name, value) in entries {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| PyValueError::new_err(format!("invalid header name: {e}")))?;
        let value = HeaderValue::from_str(&value)
            .map_err(|e| PyValueError::new_err(format!("invalid header value: {e}")))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

/// Return Sandhi-owned wire facts for a known OpenAI-compatible provider.
#[pyfunction]
#[pyo3(signature = (provider, model=None))]
fn provider_spec<'py>(
    py: Python<'py>,
    provider: &str,
    model: Option<&str>,
) -> PyResult<Bound<'py, PyDict>> {
    let spec = resolve_openai_compat_provider(provider)
        .ok_or_else(|| PyKeyError::new_err(format!("unknown provider: {provider}")))?;
    let d = PyDict::new_bound(py);
    d.set_item("slug", spec.slug)?;
    d.set_item("aliases", spec.aliases)?;
    d.set_item(
        "base_url",
        model.map_or(spec.base_url, |name| spec.base_url_for_model(name)),
    )?;
    Ok(d)
}

/// Return the versioned typed descriptor for a known provider as JSON.
#[pyfunction]
fn provider_descriptor_json(provider: &str) -> PyResult<String> {
    let descriptor = sandhi_providers::provider_descriptor(provider)
        .ok_or_else(|| PyKeyError::new_err(format!("unknown provider: {provider}")))?;
    serde_json::to_string(&descriptor)
        .map_err(|error| PyRuntimeError::new_err(format!("serialize provider descriptor: {error}")))
}

/// Return one checked chat-contract JSON Schema document.
#[pyfunction]
fn chat_contract_schema_json(name: &str) -> PyResult<String> {
    let filename = if name.ends_with(".schema.json") {
        name.to_owned()
    } else {
        format!("{name}.schema.json")
    };
    sandhi_core::contract_schema_documents()
        .remove(filename.as_str())
        .ok_or_else(|| PyKeyError::new_err(format!("unknown chat contract schema: {name}")))
}

/// The usage-event wire-contract major version this build targets.
#[pyfunction]
fn wire_contract_version() -> &'static str {
    UsageEvent::SCHEMA_VERSION
}

/// Parse a provider response body (JSON string) into the neutral token breakdown. `provider`
/// selects the parser: `anthropic` → the Anthropic Messages shape; anything else → the
/// OpenAI-compatible shape.
#[pyfunction]
fn parse_usage<'py>(
    py: Python<'py>,
    provider: &str,
    response_json: &str,
) -> PyResult<Bound<'py, PyDict>> {
    let value: serde_json::Value = serde_json::from_str(response_json)
        .map_err(|e| PyValueError::new_err(format!("response_json is not valid JSON: {e}")))?;
    let parsed = parse_for(provider, &value);
    usage_to_dict(py, &parsed)
}

/// In-process metering middleware: virtual keys, budgets, and usage-event emission.
#[pyclass]
struct Gateway {
    inner: Mutex<Inner>,
    counter: AtomicU64,
}

struct Inner {
    keys: KeyStore,
    ledger: BudgetLedger,
    events: Vec<UsageEvent>,
    jsonl_path: Option<PathBuf>,
    /// Host-registered usage parsers, by provider slug (the escape hatch).
    parsers: HashMap<String, Py<PyAny>>,
}

#[pymethods]
impl Gateway {
    /// `sink_path` — append emitted events as JSONL to this file (in addition to the in-memory
    /// buffer). `None` = in-memory only.
    #[new]
    #[pyo3(signature = (sink_path=None))]
    fn new(sink_path: Option<String>) -> Self {
        Gateway {
            inner: Mutex::new(Inner {
                keys: KeyStore::new(),
                ledger: BudgetLedger::new(),
                events: Vec::new(),
                jsonl_path: sink_path.map(PathBuf::from),
                parsers: HashMap::new(),
            }),
            counter: AtomicU64::new(0),
        }
    }

    /// Register a virtual key: `id` (what the caller presents) → subject/group attribution + an
    /// opaque `upstream` reference (never the real secret).
    #[pyo3(signature = (id, subject=None, group=None, upstream=String::new()))]
    fn add_virtual_key(
        &self,
        id: String,
        subject: Option<String>,
        group: Option<String>,
        upstream: String,
    ) {
        self.inner.lock().unwrap().keys.insert(VirtualKey {
            id,
            subject_id: subject,
            group_id: group,
            upstream_ref: upstream,
        });
    }

    /// Set a token budget on a scope (e.g. `group:platform` or `vk:vk_alice`).
    fn set_budget(&self, scope: String, tokens: u64) {
        self.inner
            .lock()
            .unwrap()
            .ledger
            .set_limit(scope, Budget::tokens(tokens));
    }

    /// Would `add` more tokens be within the scope's budget?
    fn check_budget(&self, scope: &str, add: u64) -> bool {
        self.inner.lock().unwrap().ledger.check(scope, add).is_ok()
    }

    /// Tokens spent so far on a scope.
    fn spent(&self, scope: &str) -> u64 {
        self.inner.lock().unwrap().ledger.spent(scope)
    }

    /// Register a host callback that parses a provider's response into a usage mapping with keys
    /// `{tokens_in, tokens_out, cache_creation_tokens, cache_read_tokens}`. `meter()` then uses it
    /// for that provider — the escape hatch for providers Sandhi doesn't natively parse (custom /
    /// air-gapped / community). Overrides any built-in parser for that slug.
    fn register_parser(&self, provider: String, parser: Py<PyAny>) {
        self.inner.lock().unwrap().parsers.insert(provider, parser);
    }

    /// Meter one completed call: parse usage from `response_json` (a registered host parser wins,
    /// else the built-in for `provider`), attribute it to `virtual_key`, emit the neutral event +
    /// record the budget, and return the event dict. Raises `KeyError` for an unknown virtual key,
    /// `ValueError` for bad JSON or a failing custom parser.
    #[pyo3(signature = (virtual_key, provider, model, response_json, session_id=None, route=None))]
    #[allow(clippy::too_many_arguments)]
    fn meter<'py>(
        &self,
        py: Python<'py>,
        virtual_key: &str,
        provider: &str,
        model: &str,
        response_json: &str,
        session_id: Option<String>,
        route: Option<String>,
    ) -> PyResult<Bound<'py, PyDict>> {
        // A registered host parser wins; call it *without* holding the lock (re-entrancy safety).
        let custom = self
            .inner
            .lock()
            .unwrap()
            .parsers
            .get(provider)
            .map(|p| p.clone_ref(py));
        let parsed = match custom {
            Some(cb) => {
                let out = cb.bind(py).call1((response_json,)).map_err(|e| {
                    PyValueError::new_err(format!("custom parser for '{provider}' failed: {e}"))
                })?;
                parsed_from_pyobj(&out)
            }
            None => {
                let value: serde_json::Value =
                    serde_json::from_str(response_json).map_err(|e| {
                        PyValueError::new_err(format!("response_json is not valid JSON: {e}"))
                    })?;
                parse_for(provider, &value)
            }
        };
        self.record_and_build(py, virtual_key, provider, model, parsed, session_id, route)
    }

    /// Meter from token counts you supply directly (bypass parsing entirely) — the simplest escape
    /// hatch for any provider. Same attribution + budget + emit as `meter()`.
    #[pyo3(signature = (virtual_key, provider, model, tokens_in, tokens_out,
        cache_creation_tokens=0, cache_read_tokens=0, session_id=None, route=None))]
    #[allow(clippy::too_many_arguments)]
    fn meter_tokens<'py>(
        &self,
        py: Python<'py>,
        virtual_key: &str,
        provider: &str,
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
        cache_creation_tokens: u64,
        cache_read_tokens: u64,
        session_id: Option<String>,
        route: Option<String>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let parsed = ParsedUsage {
            tokens_in,
            tokens_out,
            cache_creation_tokens,
            cache_read_tokens,
        };
        self.record_and_build(py, virtual_key, provider, model, parsed, session_id, route)
    }

    /// All events emitted so far (in-memory), as dicts.
    fn events<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let inner = self.inner.lock().unwrap();
        inner.events.iter().map(|e| event_to_dict(py, e)).collect()
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

    /// Shared tail: resolve the key, build + emit the event, record the budget, return the dict.
    #[allow(clippy::too_many_arguments)]
    fn record_and_build<'py>(
        &self,
        py: Python<'py>,
        virtual_key: &str,
        provider: &str,
        model: &str,
        parsed: ParsedUsage,
        session_id: Option<String>,
        route: Option<String>,
    ) -> PyResult<Bound<'py, PyDict>> {
        let mut inner = self.inner.lock().unwrap();
        let vk =
            inner.keys.resolve(virtual_key).cloned().ok_or_else(|| {
                PyKeyError::new_err(format!("unknown virtual key: {virtual_key}"))
            })?;

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

        event_to_dict(py, &event)
    }
}

/// Extract a `ParsedUsage` from a Python mapping returned by a host parser callback (missing keys
/// default to 0).
fn parsed_from_pyobj(obj: &Bound<'_, PyAny>) -> ParsedUsage {
    let get = |k: &str| -> u64 {
        obj.get_item(k)
            .ok()
            .and_then(|v| v.extract::<u64>().ok())
            .unwrap_or(0)
    };
    ParsedUsage {
        tokens_in: get("tokens_in"),
        tokens_out: get("tokens_out"),
        cache_creation_tokens: get("cache_creation_tokens"),
        cache_read_tokens: get("cache_read_tokens"),
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

fn usage_to_dict<'py>(py: Python<'py>, u: &ParsedUsage) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new_bound(py);
    d.set_item("tokens_in", u.tokens_in)?;
    d.set_item("tokens_out", u.tokens_out)?;
    d.set_item("cache_creation_tokens", u.cache_creation_tokens)?;
    d.set_item("cache_read_tokens", u.cache_read_tokens)?;
    Ok(d)
}

fn event_to_dict<'py>(py: Python<'py>, e: &UsageEvent) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new_bound(py);
    d.set_item("schema_version", &e.schema_version)?;
    d.set_item("request_id", &e.request_id)?;
    d.set_item("occurred_at", &e.occurred_at)?;
    d.set_item("provider", &e.provider)?;
    d.set_item("model", &e.model)?;
    d.set_item(
        "backend",
        match e.backend {
            Backend::External => "external",
            Backend::SelfHosted => "self_hosted",
        },
    )?;
    d.set_item("virtual_key_id", e.virtual_key_id.clone())?;
    d.set_item("subject_id", e.subject_id.clone())?;
    d.set_item("group_id", e.group_id.clone())?;
    d.set_item("route", e.route.clone())?;
    d.set_item("session_id", e.session_id.clone())?;
    d.set_item("tokens_in", e.tokens_in)?;
    d.set_item("tokens_out", e.tokens_out)?;
    d.set_item("cache_creation_tokens", e.cache_creation_tokens)?;
    d.set_item("cache_read_tokens", e.cache_read_tokens)?;
    d.set_item(
        "usage_completeness",
        match e.usage_completeness {
            sandhi_core::UsageCompleteness::Final => "final",
            sandhi_core::UsageCompleteness::Partial => "partial",
            sandhi_core::UsageCompleteness::Unavailable => "unavailable",
        },
    )?;
    d.set_item("attempts", e.attempts)?;
    d.set_item("outcome", e.outcome.clone())?;
    d.set_item("upstream_request_id", e.upstream_request_id.clone())?;
    d.set_item("gpu_seconds", e.gpu_seconds)?;
    Ok(d)
}

#[pymodule]
fn sandhi_gateway(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add(
        "__doc__",
        "Sandhi — the metering layer for AI agents (in-process Python middleware).",
    )?;
    m.add_function(wrap_pyfunction!(wire_contract_version, m)?)?;
    m.add_function(wrap_pyfunction!(parse_usage, m)?)?;
    m.add_function(wrap_pyfunction!(provider_spec, m)?)?;
    m.add_function(wrap_pyfunction!(provider_descriptor_json, m)?)?;
    m.add_function(wrap_pyfunction!(chat_contract_schema_json, m)?)?;
    m.add_class::<PyProviderRuntime>()?;
    m.add_class::<TypedProvider>()?;
    m.add_class::<TypedEventStreamIter>()?;
    m.add_class::<Gateway>()?;
    Ok(())
}
