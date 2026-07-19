//! Sandhi Node binding (napi-rs) — the **in-process metering middleware** for
//! TypeScript/JavaScript, published to npm as `@anvai-labs/sandhi`. Mirrors the Python
//! `sandhi_gateway` API; depends only on `sandhi-core` (no HTTP transport in the addon).
//!
//! JS API (napi camel-cases the Rust names): `wireContractVersion()`, `parseUsage()`, and a
//! `Gateway` class with `addVirtualKey / setBudget / checkBudget / spent / meter / events`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use sandhi_core::{
    parse_anthropic_usage, parse_openai_usage, Backend, Budget, BudgetLedger, KeyStore,
    ParsedUsage, UsageEvent, VirtualKey,
};

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

    /// Meter one completed call: parse usage from `responseJson`, attribute it to `virtualKey`,
    /// emit the neutral event + record the budget, and return the event. Throws on an unknown
    /// virtual key or bad JSON.
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

        let mut inner = self.inner.lock().unwrap();
        let vk = inner
            .keys
            .resolve(&virtual_key)
            .cloned()
            .ok_or_else(|| Error::from_reason(format!("unknown virtual key: {virtual_key}")))?;

        let event = parsed.apply(
            UsageEvent::new(
                self.next_request_id(),
                now_rfc3339(),
                &provider,
                &model,
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
}

fn parse_for(provider: &str, value: &serde_json::Value) -> ParsedUsage {
    match provider {
        "anthropic" => parse_anthropic_usage(value),
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
        gpu_seconds: e.gpu_seconds,
    }
}
