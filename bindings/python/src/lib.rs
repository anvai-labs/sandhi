//! Sandhi Python binding (PyO3) — the **in-process metering middleware** (AnvaiOps ADR-0047
//! D2/D10). Published to PyPI as `sandhi-gateway`, imported as `sandhi_gateway`.
//!
//! A caller (e.g. victor) keeps making its own provider calls, then hands the raw response to
//! [`Gateway::meter`]: Sandhi parses the usage **at the source** (same Rust parsers as the
//! proxy), attributes it to a virtual key's subject/group, enforces + records the budget, emits
//! the neutral usage event, and returns it for local display. Zero network hop.
//!
//! Depends only on `sandhi-core` — the HTTP transport (`sandhi-providers`) is not pulled into
//! the wheel.

// pyo3's #[pyfunction]/#[pymethods] macros emit `.into()` on the PyErr return path; on this
// pyo3 + clippy combo that trips `useless_conversion` inside generated code (not our code).
#![allow(clippy::useless_conversion)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pyo3::exceptions::{PyKeyError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyDict};

use sandhi_core::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_usage, Backend, Budget, BudgetLedger, KeyStore, ParsedUsage,
    UsageEvent, VirtualKey,
};
use sandhi_providers::{
    Anthropic, Cohere, Gemini, Ollama, OpenAiCompat, Provider, ProviderError, ProviderRequest,
};

/// Build a provider adapter from its neutral slug + endpoint. OpenAI-compatible providers (OpenAI,
/// Azure, Groq, vLLM, …) all use `OpenAiCompat` with the slug preserved for metering; Anthropic /
/// Cohere / Gemini / Ollama have dedicated adapters. Transport step 3 (ADR-0047 D10).
fn build_provider(provider: &str, base_url: &str, api_key: &str) -> Box<dyn Provider> {
    match provider {
        "anthropic" => Box::new(Anthropic::new(base_url, api_key)),
        "cohere" => Box::new(Cohere::new(base_url, api_key)),
        "gemini" => Box::new(Gemini::new(base_url, api_key)),
        "ollama" => Box::new(Ollama::new(base_url)),
        // openai + every OpenAI-compatible slug: one adapter, slug preserved.
        _ => Box::new(OpenAiCompat::new(provider.to_string(), base_url, api_key)),
    }
}

fn provider_err_to_py(e: ProviderError) -> PyErr {
    PyRuntimeError::new_err(format!("sandhi transport: {e}"))
}

/// Forward one **non-streaming** provider call through sandhi's in-process transport (ADR-0047 D10
/// step 3a). Returns a Python **awaitable** resolving to `{status, body, usage}` — `usage` is parsed
/// at the source by sandhi (single-sourced metering trust). `provider` is the neutral slug; `body`
/// is the provider-native request JSON, forwarded prefix-exact; `session_id` is preserved
/// end-to-end for prompt-cache / KV affinity (ADR-0047 D9).
#[pyfunction]
#[pyo3(signature = (provider, model, base_url, api_key, body_json, session_id=None))]
fn complete<'py>(
    py: Python<'py>,
    provider: String,
    model: String,
    base_url: String,
    api_key: String,
    body_json: String,
    session_id: Option<String>,
) -> PyResult<Bound<'py, PyAny>> {
    let body: serde_json::Value = serde_json::from_str(&body_json)
        .map_err(|e| PyValueError::new_err(format!("body_json is not valid JSON: {e}")))?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let adapter = build_provider(&provider, &base_url, &api_key);
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
    m.add_class::<Gateway>()?;
    Ok(())
}
