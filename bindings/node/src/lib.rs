//! Sandhi Node binding (napi-rs) — **in-process metering middleware + provider transport** for
//! TypeScript/JavaScript, published to npm as `@anvai-labs/sandhi`. Mirrors the Python
//! `sandhi_gateway` API.
//!
//! Two surfaces (napi camel-cases the Rust names):
//! - **Metering**: `wireContractVersion()`, `parseUsage()`, and a `Gateway` class
//!   (`addVirtualKey / setBudget / checkBudget / spent / meter / meterTokens / events`).
//! - **Transport** (ADR-0047 D10 step 3c): `complete()` returns a `Promise<CompleteResult>`;
//!   `stream()` returns a `Promise<ByteStream>` whose `read()` yields chunks (`{ data, usage }`,
//!   `null` at end) — forward bytes verbatim (D9), usage finalized on the terminal chunk. A tiny
//!   `Symbol.asyncIterator` shim in `sandhi.js` turns `ByteStream` into an `for await` iterable.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use napi::bindgen_prelude::*;
use napi_derive::napi;

use sandhi_core::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_usage, Backend, Budget, BudgetLedger, KeyStore, ParsedUsage,
    UsageEvent, VirtualKey,
};
use sandhi_providers::{
    Anthropic, Cohere, Gemini, Ollama, OpenAiCompat, Provider, ProviderError, ProviderRequest,
    StreamChunk,
};

/// Build a provider adapter from its neutral slug + endpoint (mirrors the Python binding's
/// `build_provider`). OpenAI-compatible providers all use `OpenAiCompat` with the slug preserved;
/// Anthropic / Cohere / Gemini / Ollama have dedicated adapters. Transport step 3 (ADR-0047 D10).
fn build_provider(provider: &str, base_url: &str, api_key: &str) -> Box<dyn Provider> {
    match provider {
        "anthropic" => Box::new(Anthropic::new(base_url, api_key)),
        "cohere" => Box::new(Cohere::new(base_url, api_key)),
        "gemini" => Box::new(Gemini::new(base_url, api_key)),
        "ollama" => Box::new(Ollama::new(base_url)),
        _ => Box::new(OpenAiCompat::new(provider.to_string(), base_url, api_key)),
    }
}

fn provider_err_to_napi(e: ProviderError) -> Error {
    Error::from_reason(format!("sandhi transport: {e}"))
}

/// A completed (non-streaming) provider response: `body` is the provider-native JSON (as a string),
/// `usage` is parsed at the source. Mirrors the Python `complete()` return.
#[napi(object)]
pub struct CompleteResult {
    pub status: u32,
    pub body: String,
    pub usage: UsageBreakdown,
}

/// Forward one **non-streaming** provider call through sandhi's in-process transport (ADR-0047 D10
/// step 3c). `provider` is the neutral slug; `bodyJson` is the provider-native request JSON,
/// forwarded prefix-exact; `sessionId` is preserved for prompt-cache / KV affinity (D9).
#[napi]
pub async fn complete(
    provider: String,
    model: String,
    base_url: String,
    api_key: String,
    body_json: String,
    session_id: Option<String>,
) -> Result<CompleteResult> {
    let body: serde_json::Value = serde_json::from_str(&body_json)
        .map_err(|e| Error::from_reason(format!("bodyJson is not valid JSON: {e}")))?;
    let adapter = build_provider(&provider, &base_url, &api_key);
    let req = ProviderRequest::new(model, body).with_session(session_id);
    let resp = adapter.complete(req).await.map_err(provider_err_to_napi)?;
    let body_str =
        serde_json::to_string(&resp.body).map_err(|e| Error::from_reason(e.to_string()))?;
    Ok(CompleteResult {
        status: u32::from(resp.status),
        body: body_str,
        usage: usage_breakdown(&resp.usage),
    })
}

/// One streaming chunk: `data` is raw upstream bytes to forward verbatim; `usage` is populated only
/// on the terminal chunk (mirrors the Python `stream()` items).
#[napi(object)]
pub struct StreamChunkJs {
    pub data: Buffer,
    pub usage: Option<UsageBreakdown>,
}

/// One item pushed over the channel from the background stream driver to `ByteStream.read`.
struct StreamItem {
    data: Vec<u8>,
    usage: Option<ParsedUsage>,
}

/// A streaming provider response (ADR-0047 D10 step 3c). A background task drives the Rust
/// `ByteStream` and pushes chunks into a bounded channel (backpressure ⇒ O(1) memory, D7);
/// `read()` awaits the next chunk and resolves to `null` when the stream is exhausted. The
/// `sandhi.js` shim adds `Symbol.asyncIterator` so this is usable with `for await`.
#[napi]
pub struct ByteStream {
    rx: Arc<
        tokio::sync::Mutex<tokio::sync::mpsc::Receiver<std::result::Result<StreamItem, String>>>,
    >,
}

#[napi]
impl ByteStream {
    /// Await the next chunk; resolves to `null` once the stream is exhausted.
    #[napi]
    pub async fn read(&self) -> Result<Option<StreamChunkJs>> {
        let rx = self.rx.clone();
        let mut guard = rx.lock().await;
        match guard.recv().await {
            Some(Ok(item)) => Ok(Some(StreamChunkJs {
                data: item.data.into(),
                usage: item.usage.as_ref().map(usage_breakdown),
            })),
            Some(Err(e)) => Err(Error::from_reason(format!("sandhi stream: {e}"))),
            None => Ok(None),
        }
    }
}

/// Forward one **streaming** provider call through sandhi's in-process transport (ADR-0047 D10 step
/// 3c). Resolves to a [`ByteStream`]; bytes are forwarded verbatim (prefix-exact, D9), usage is
/// finalized on the terminal chunk, and `sessionId` is preserved for prompt-cache / KV affinity.
#[napi]
pub async fn stream(
    provider: String,
    model: String,
    base_url: String,
    api_key: String,
    body_json: String,
    session_id: Option<String>,
) -> Result<ByteStream> {
    let body: serde_json::Value = serde_json::from_str(&body_json)
        .map_err(|e| Error::from_reason(format!("bodyJson is not valid JSON: {e}")))?;
    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<StreamItem, String>>(64);
    // Drive the stream on napi's tokio runtime; `read()` pulls from the channel independently.
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let adapter = build_provider(&provider, &base_url, &api_key);
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
                        Err(e) => (Err(e.to_string()), true),
                    };
                    if tx.send(msg).await.is_err() || stop {
                        break;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string())).await;
            }
        }
    });
    Ok(ByteStream {
        rx: Arc::new(tokio::sync::Mutex::new(rx)),
    })
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
