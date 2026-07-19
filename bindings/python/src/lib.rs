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

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use sandhi_core::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_usage, Backend, Budget, BudgetLedger, KeyStore, ParsedUsage,
    UsageEvent, VirtualKey,
};

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

    /// Meter one completed call: parse usage from `response_json`, attribute it to
    /// `virtual_key`, emit the neutral event + record the budget, and return the event dict.
    /// Raises `KeyError` for an unknown virtual key, `ValueError` for bad JSON.
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
        let value: serde_json::Value = serde_json::from_str(response_json)
            .map_err(|e| PyValueError::new_err(format!("response_json is not valid JSON: {e}")))?;
        let parsed = parse_for(provider, &value);

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

        // Emit: append JSONL (best-effort) + in-memory buffer.
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
    m.add_class::<Gateway>()?;
    Ok(())
}
