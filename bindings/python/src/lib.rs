//! Sandhi Python binding (PyO3) — **in-process metering middleware + provider transport** (AnvaiOps
//! ADR-0047 D2/D10). Published to PyPI as `sandhi-gateway`, imported as `sandhi_gateway`.
//!
//! Two surfaces:
//! - **Metering** ([`Gateway`], [`parse_usage`]): a caller keeps making its own provider calls, then
//!   hands the raw response over; Sandhi parses the usage **at the source** (same Rust parsers as the
//!   proxy), attributes it to a virtual key, enforces + records the budget, and emits the neutral
//!   usage event. Zero network hop.
//! - **Transport** ([`complete`] / [`stream`], ADR-0047 D10 step 3): forward a provider call through
//!   sandhi-providers' async transport **in-process** — `complete` returns an awaitable, `stream` an
//!   async iterator (bytes forwarded verbatim, D9; usage finalized on the terminal item). This makes
//!   Sandhi the shared network core the in-house apps can `import`. [`register_provider`] is the D10
//!   escape hatch: a host-language (Python) async callable registers as a custom provider, so a
//!   custom / air-gapped / community provider rides `complete()` without a Rust adapter.
//!
//! Depends on `sandhi-core` (metering) + `sandhi-providers` (transport) — the latter pulls the
//! async HTTP stack into the wheel to serve the transport surface.

// pyo3's #[pyfunction]/#[pymethods] macros emit `.into()` on the PyErr return path; on this
// pyo3 + clippy combo that trips `useless_conversion` inside generated code (not our code).
#![allow(clippy::useless_conversion)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pyo3::exceptions::{
    PyKeyError, PyRuntimeError, PyStopAsyncIteration, PyTimeoutError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyDict};
use std::sync::Arc;

use sandhi_core::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_usage, Backend, Budget, BudgetLedger, KeyStore, ParsedUsage,
    UsageEvent, VirtualKey,
};
use sandhi_providers::{
    Anthropic, CircuitBreaker, Cohere, FnProvider, Gemini, Ollama, OpenAiCompat, Provider,
    ProviderError, ProviderRequest, ProviderResponse, ResilientProvider, StreamChunk,
    TimeoutConfig,
};
use std::sync::OnceLock;

/// Host-registered custom providers (ADR-0047 D10 escape hatch): slug → a Python async callable that
/// owns its own transport. Consulted before the built-in adapters, so a custom / air-gapped /
/// community provider works through `complete()` without a Rust contribution.
static CUSTOM_PROVIDERS: OnceLock<Mutex<HashMap<String, Py<PyAny>>>> = OnceLock::new();

fn custom_providers() -> &'static Mutex<HashMap<String, Py<PyAny>>> {
    CUSTOM_PROVIDERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Build a provider adapter from its neutral slug + endpoint. A host-registered Python provider
/// (D10 escape hatch) wins over the built-ins; otherwise OpenAI-compatible providers (OpenAI,
/// Azure, Groq, vLLM, …) all use `OpenAiCompat` with the slug preserved for metering, and Anthropic
/// / Cohere / Gemini / Ollama have dedicated adapters. Transport step 3 (ADR-0047 D10).
fn build_provider(
    provider: &str,
    base_url: &str,
    api_key: &str,
    opts: &TransportOpts,
) -> Arc<dyn Provider> {
    // Escape hatch: a host-registered Python provider takes precedence (clone the handle out from
    // under the lock, then dispatch through `FnProvider`).
    let registered = custom_providers()
        .lock()
        .unwrap()
        .get(provider)
        .map(|h| Python::with_gil(|py| h.clone_ref(py)));
    let bare: Arc<dyn Provider> = if let Some(handler) = registered {
        Arc::new(python_fn_provider(provider.to_string(), handler))
    } else {
        match provider {
            "anthropic" => Arc::new(Anthropic::new(base_url, api_key)),
            "cohere" => Arc::new(Cohere::new(base_url, api_key)),
            "gemini" => Arc::new(Gemini::new(base_url, api_key)),
            "ollama" => Arc::new(Ollama::new(base_url)),
            // openai + every OpenAI-compatible slug: one adapter, slug preserved.
            _ => Arc::new(OpenAiCompat::new(provider.to_string(), base_url, api_key)),
        }
    };
    // Uniform decorator wrap: retry + shared breaker + timeouts, for built-ins AND the escape
    // hatch (a bare transport would carry fewer guarantees than a direct client — ADR-0002).
    let mut resilient =
        ResilientProvider::new(bare).with_shared_breaker(shared_breaker(provider, base_url));
    if let Some(max_retries) = opts.max_retries {
        resilient = resilient.with_retry(max_retries, std::time::Duration::from_millis(200));
    }
    let mut timeouts = TimeoutConfig::default();
    if let Some(secs) = opts.timeout_secs {
        timeouts.complete = std::time::Duration::from_secs_f64(secs.max(0.001));
    }
    if let Some(secs) = opts.stream_idle_timeout_secs {
        timeouts.idle = Some(std::time::Duration::from_secs_f64(secs.max(0.001)));
    }
    Arc::new(resilient.with_timeouts(timeouts))
}

/// Additive per-call transport knobs (`None` → the Rust defaults).
#[derive(Default)]
struct TransportOpts {
    timeout_secs: Option<f64>,
    stream_idle_timeout_secs: Option<f64>,
    max_retries: Option<u32>,
}

/// One circuit breaker per `(provider, base_url)` upstream: `build_provider` constructs a
/// provider per call, and a per-call breaker would be stateless theater.
static BREAKERS: OnceLock<Mutex<HashMap<(String, String), Arc<CircuitBreaker>>>> = OnceLock::new();

fn shared_breaker(provider: &str, base_url: &str) -> Arc<CircuitBreaker> {
    let mut map = BREAKERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    map.entry((provider.to_string(), base_url.to_string()))
        .or_insert_with(|| Arc::new(CircuitBreaker::new(5, std::time::Duration::from_secs(30))))
        .clone()
}

/// Wrap a host-registered Python async callable as a [`Provider`] (ADR-0047 D10 escape hatch). The
/// closure calls the Python coroutine and bridges it back to a Rust future (the reverse of
/// [`future_into_py`]: `pyo3_async_runtimes::tokio::into_future`), then parses its returned mapping
/// into a [`ProviderResponse`]. `stream()` is unsupported for custom providers (the underlying
/// `FnProvider` returns 501), mirroring the Rust escape hatch.
fn python_fn_provider(slug: String, handler: Py<PyAny>) -> FnProvider {
    FnProvider::new(slug, move |req: ProviderRequest| {
        let handler = Python::with_gil(|py| handler.clone_ref(py));
        async move {
            // Call the Python handler → coroutine, then turn that awaitable into a Rust future.
            let fut = Python::with_gil(|py| -> PyResult<_> {
                let body_json = serde_json::to_string(&req.body)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
                let coro = handler.bind(py).call1((
                    req.model.clone(),
                    body_json,
                    req.session_id.clone(),
                ))?;
                pyo3_async_runtimes::tokio::into_future(coro)
            })
            .map_err(|e| ProviderError::Transport(format!("custom provider dispatch: {e}")))?;
            let result = fut
                .await
                .map_err(|e| ProviderError::Transport(format!("custom provider errored: {e}")))?;
            Python::with_gil(|py| py_obj_to_response(result.bind(py)))
        }
    })
}

/// Parse a custom provider's returned mapping into a [`ProviderResponse`]. Contract:
/// `{"status": int, "body": <JSON string>, "usage": {tokens_in, tokens_out, …} | None}`.
fn py_obj_to_response(obj: &Bound<'_, PyAny>) -> Result<ProviderResponse, ProviderError> {
    let te = |m: String| ProviderError::Transport(m);
    let status: u16 = obj
        .get_item("status")
        .map_err(|e| te(format!("custom provider result missing 'status': {e}")))?
        .extract()
        .map_err(|e| te(format!("custom provider 'status' not an int: {e}")))?;
    let body_str: String = obj
        .get_item("body")
        .map_err(|e| te(format!("custom provider result missing 'body': {e}")))?
        .extract()
        .map_err(|e| te(format!("custom provider 'body' must be a JSON string: {e}")))?;
    let body: serde_json::Value = serde_json::from_str(&body_str)
        .map_err(|e| te(format!("custom provider 'body' is not valid JSON: {e}")))?;
    let usage = match obj.get_item("usage") {
        Ok(u) if !u.is_none() => parsed_from_pyobj(&u),
        _ => ParsedUsage::default(),
    };
    Ok(ProviderResponse {
        status,
        body,
        usage,
    })
}

/// Register a Python async callable as a custom provider under `slug` (ADR-0047 D10 escape hatch —
/// the host-language adapter). The handler is
/// `async def handler(model: str, body_json: str, session_id: str | None) -> dict` returning
/// `{"status": int, "body": <JSON string>, "usage": {tokens_in, tokens_out, cache_creation_tokens,
/// cache_read_tokens} | None}`; it owns its own transport, so a custom / air-gapped / community
/// provider works through `complete()` without a Rust adapter. Overrides a built-in slug of the same
/// name. Streaming is not supported for custom providers (mirrors the Rust `FnProvider`).
#[pyfunction]
fn register_provider(slug: String, handler: Py<PyAny>) {
    custom_providers().lock().unwrap().insert(slug, handler);
}

fn provider_err_to_py(e: ProviderError) -> PyErr {
    match e {
        ProviderError::Timeout(_) => PyTimeoutError::new_err(format!("sandhi transport: {e}")),
        _ => PyRuntimeError::new_err(format!("sandhi transport: {e}")),
    }
}

/// Forward one **non-streaming** provider call through sandhi's in-process transport (ADR-0047 D10
/// step 3a). Returns a Python **awaitable** resolving to `{status, body, usage}` — `usage` is parsed
/// at the source by sandhi (single-sourced metering trust). `provider` is the neutral slug; `body`
/// is the provider-native request JSON, forwarded prefix-exact; `session_id` is preserved
/// end-to-end for prompt-cache / KV affinity (ADR-0047 D9).
#[pyfunction]
#[allow(clippy::too_many_arguments)] // pyo3 signature: flat kwargs are the FFI contract
#[pyo3(signature = (provider, model, base_url, api_key, body_json, session_id=None, timeout_secs=None, stream_idle_timeout_secs=None, max_retries=None))]
fn complete<'py>(
    py: Python<'py>,
    provider: String,
    model: String,
    base_url: String,
    api_key: String,
    body_json: String,
    session_id: Option<String>,
    timeout_secs: Option<f64>,
    stream_idle_timeout_secs: Option<f64>,
    max_retries: Option<u32>,
) -> PyResult<Bound<'py, PyAny>> {
    let body: serde_json::Value = serde_json::from_str(&body_json)
        .map_err(|e| PyValueError::new_err(format!("body_json is not valid JSON: {e}")))?;
    let opts = TransportOpts {
        timeout_secs,
        stream_idle_timeout_secs,
        max_retries,
    };
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let adapter = build_provider(&provider, &base_url, &api_key, &opts);
        let req = ProviderRequest::new(model, body).with_session(session_id);
        let resp = adapter.complete(req).await.map_err(provider_err_to_py)?;
        Python::with_gil(|py| -> PyResult<Py<PyAny>> {
            let d = PyDict::new_bound(py);
            d.set_item("status", resp.status)?;
            let body_str = serde_json::to_string(&resp.body)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            d.set_item("body", body_str)?;
            d.set_item("usage", usage_to_dict(py, &resp.usage)?)?;
            Ok(d.into_any().unbind())
        })
    })
}

/// One item yielded by [`ByteStreamIter`]: raw upstream bytes to forward verbatim, plus (on the
/// terminal item only) the finalized usage.
struct StreamItem {
    data: Vec<u8>,
    usage: Option<ParsedUsage>,
}

/// A Python **async iterator** over a streaming provider response (ADR-0047 D10 step 3b). A
/// background tokio task drives the Rust `ByteStream` and pushes chunks into a bounded channel
/// (backpressure ⇒ O(1) memory, D7); `__anext__` awaits the next chunk. Yields dicts
/// `{"data": bytes, "usage": dict|None}` — `usage` is populated only on the terminal item.
#[pyclass]
struct ByteStreamIter {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Result<StreamItem, String>>>>,
}

#[pymethods]
impl ByteStreamIter {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.rx.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut guard = rx.lock().await;
            match guard.recv().await {
                Some(Ok(item)) => Python::with_gil(|py| -> PyResult<Py<PyAny>> {
                    let d = PyDict::new_bound(py);
                    d.set_item("data", PyBytes::new_bound(py, &item.data))?;
                    match &item.usage {
                        Some(u) => d.set_item("usage", usage_to_dict(py, u)?)?,
                        None => d.set_item("usage", py.None())?,
                    }
                    Ok(d.into_any().unbind())
                }),
                Some(Err(e)) => Err(PyRuntimeError::new_err(format!("sandhi stream: {e}"))),
                None => Err(PyStopAsyncIteration::new_err(())),
            }
        })
    }
}

/// Forward one **streaming** provider call through sandhi's in-process transport (ADR-0047 D10
/// step 3b). Returns a Python **async iterator** (`async for chunk in ...`) yielding
/// `{"data": bytes, "usage": dict|None}` — bytes are forwarded verbatim (prefix-exact, D9), usage
/// finalized on the terminal item. `session_id` is preserved for prompt-cache / KV affinity.
#[pyfunction]
#[allow(clippy::too_many_arguments)] // pyo3 signature: flat kwargs are the FFI contract
#[pyo3(signature = (provider, model, base_url, api_key, body_json, session_id=None, timeout_secs=None, stream_idle_timeout_secs=None, max_retries=None))]
fn stream(
    provider: String,
    model: String,
    base_url: String,
    api_key: String,
    body_json: String,
    session_id: Option<String>,
    timeout_secs: Option<f64>,
    stream_idle_timeout_secs: Option<f64>,
    max_retries: Option<u32>,
) -> PyResult<ByteStreamIter> {
    let body: serde_json::Value = serde_json::from_str(&body_json)
        .map_err(|e| PyValueError::new_err(format!("body_json is not valid JSON: {e}")))?;
    let opts = TransportOpts {
        timeout_secs,
        stream_idle_timeout_secs,
        max_retries,
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamItem, String>>(64);
    pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
        use futures_util::StreamExt;
        let adapter = build_provider(&provider, &base_url, &api_key, &opts);
        let req = ProviderRequest::new(model, body).with_session(session_id);
        match adapter.stream(req).await {
            Ok(mut s) => {
                while let Some(chunk) = s.next().await {
                    let (msg, stop) = match chunk {
                        Ok(StreamChunk { data, usage }) => (
                            Ok(StreamItem {
                                data: data.to_vec(),
                                usage,
                            }),
                            false,
                        ),
                        Err(e) => (Err(e.to_string()), true), // forward the error, then stop
                    };
                    if tx.send(msg).await.is_err() || stop {
                        break; // receiver dropped, or the terminal error was forwarded
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string())).await;
            }
        }
        // tx drops here → channel closes → __anext__ recv returns None → StopAsyncIteration
    });
    Ok(ByteStreamIter {
        rx: Arc::new(tokio::sync::Mutex::new(rx)),
    })
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
    m.add_function(wrap_pyfunction!(complete, m)?)?;
    m.add_function(wrap_pyfunction!(stream, m)?)?;
    m.add_function(wrap_pyfunction!(register_provider, m)?)?;
    m.add_class::<ByteStreamIter>()?;
    m.add_class::<Gateway>()?;
    Ok(())
}
