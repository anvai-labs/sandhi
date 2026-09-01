//! Sandhi unified provider transport (AnvaiOps ADR-0047 D10).
//!
//! One Rust implementation of the provider wire layer that victor, ProximaDB, and AnvaiOps
//! all delegate to — because usage/cache-token parsing is provider-specific and must be
//! **single-sourced at the point of the call**, where metering trust is decided.
//!
//! Patterns: **adapter** (per provider), **strategy** (routing/fallback), **factory** (from
//! config), **decorator** (metering + circuit-breaker + retry — later, wrapped around each
//! adapter). OpenAI-compatibility covers ~20 providers, so the real adapter surface is small.
//!
//! Adapters return raw counts ([`ParsedUsage`]) + the response; the **caller** (proxy /
//! middleware) assembles the neutral [`sandhi_core::UsageEvent`] with request id, timestamp,
//! and attribution — the adapter never fabricates those.

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;

// The usage parsers are metering primitives — they live in `sandhi-core` (no transport deps).
pub use sandhi_core::usage::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_responses_usage, parse_openai_usage, ParsedUsage,
};

pub mod anthropic;
mod anthropic_typed;
pub mod catalog;
pub mod cohere;
mod cohere_typed;
pub mod escape_hatch;
pub mod gemini;
mod gemini_typed;
pub mod local;
mod ollama_typed;
pub mod openai;
pub mod openai_responses;
mod openai_responses_typed;
pub mod openai_roles;
pub mod raw;
pub mod resilience;
pub mod typed;
pub use anthropic::{Anthropic, AnthropicAuthScheme};
pub use catalog::{
    openai_compat_descriptor, provider_descriptor, resolve_openai_compat_provider,
    ModelEndpointRoute, OpenAiCompatProviderSpec, OPENAI_COMPAT_PROVIDER_SPECS,
};
pub use cohere::Cohere;
pub use escape_hatch::FnProvider;
pub use gemini::{Gemini, GeminiAuthScheme};
pub use local::Ollama;
pub use openai::OpenAiCompat;
pub use openai_responses::{OpenAiResponses, OpenAiResponsesProfile};
pub use openai_roles::{validate_openai_chat_messages, OpenAiChatRole};
mod linesplit;
pub mod metering;
pub use metering::MeteredProvider;
pub use resilience::{CircuitBreaker, ResilientProvider, RetryConfig, TimeoutConfig};
pub use typed::{
    ChatEventStream, ChatProvider, FamilyFacts, ProviderFamily, ProviderHandle, ProviderRuntime,
    ProviderTransportConfig, UsageCadence,
};

/// Shared HTTP client for the in-repo adapters: a 10s TCP/TLS connect bound as
/// defense-in-depth under the decorator's per-attempt timeouts. Policy timeouts
/// (whole-call / stream-setup / idle) live in [`TimeoutConfig`], not here.
pub(crate) fn default_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// AWS Bedrock — the usage parser is [`sandhi_core::usage::parse_bedrock_usage`]. Native
/// transport needs AWS **SigV4** request signing (a dedicated follow-up); until then, front
/// Bedrock with an OpenAI-compatible gateway and use [`OpenAiCompat`].
pub mod bedrock {}

/// A model request. `body` is the provider-native JSON, forwarded prefix-exact so prompt
/// caches keep hitting (ADR-0047 D9). `session_id` is the conversation key for cache/KV
/// affinity — preserved end-to-end, never flattened.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub model: String,
    pub body: serde_json::Value,
    pub session_id: Option<String>,
    /// Per-call wire headers (TD-0022 D1) — gateway-path metadata that changes per turn
    /// (`x-sandhi-run-id` / `-step-id`, vendor correlation ids), which cannot be fixed at
    /// transport construction without rebuilding the handle every turn. Merged OVER the
    /// transport's static headers by [`merge_call_headers`], which strips transport-owned
    /// names. Never enters the wire body.
    pub extra_headers: http::HeaderMap,
    /// Who this call is for (metering decorator input). Never enters the wire body —
    /// attribution rides outside the cached prompt (ADR-0001 §4); adapters ignore it.
    pub attribution: Attribution,
}

/// Per-call attribution consumed by the metering decorator. Carried on the request (not the
/// decorator constructor) because one provider instance serves many virtual keys in the proxy.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    pub virtual_key_id: Option<String>,
    pub subject_id: Option<String>,
    pub group_id: Option<String>,
    pub route: Option<String>,
}

impl ProviderRequest {
    pub fn new(model: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            model: model.into(),
            body,
            session_id: None,
            extra_headers: http::HeaderMap::new(),
            attribution: Attribution::default(),
        }
    }

    #[must_use]
    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    /// Per-call wire headers (TD-0022 D1). Transport-owned names are stripped by
    /// [`merge_call_headers`] at send time, not here — the request is data, the strip is
    /// transport defense.
    #[must_use]
    pub fn with_extra_headers(mut self, extra_headers: http::HeaderMap) -> Self {
        self.extra_headers = extra_headers;
        self
    }

    #[must_use]
    pub fn with_attribution(mut self, attribution: Attribution) -> Self {
        self.attribution = attribution;
        self
    }
}

/// The request header name this vendor uses for per-request correlation, when it declares
/// one (ADR-0008 D6). The CALLER owns the value and the injection (per-call wire headers,
/// TD-0022 D1): the proxy mints the id at admission and sends the same string that becomes
/// the usage event's `request_id`, so upstream logs and sandhi events correlate 1:1.
#[must_use]
pub fn client_request_id_header(slug: &str) -> Option<&'static str> {
    crate::resolve_openai_compat_provider(slug).and_then(|spec| spec.client_request_id_header)
}

/// Transport-owned header names, single-sourced (TD-0022 D2): the generic credential/framing
/// set plus the family credential headers and the Anthropic protocol version. A caller-supplied
/// set (static `with_headers` or per-call) can never override any of these — for the family
/// credential headers the stakes are the vaulted key itself: reqwest **appends** values for
/// names added after a header map, so an unstripped `x-api-key` in a caller set would put a
/// second, attacker-supplied credential header on the wire next to the real one.
const TRANSPORT_OWNED_HEADERS: [&str; 7] = [
    "authorization",
    "host",
    "content-type",
    "accept-encoding",
    // Family credential headers (Anthropic / Gemini) — never caller-suppliable.
    "x-api-key",
    "x-goog-api-key",
    // Protocol version is transport-owned by the Anthropic adapter.
    "anthropic-version",
];

/// Strip transport-owned header names from a caller-supplied set — the defense applied to
/// both static (`with_headers`) and per-call headers, single-sourced here (TD-0022 D2).
#[must_use]
pub fn strip_transport_owned(mut headers: http::HeaderMap) -> http::HeaderMap {
    for name in TRANSPORT_OWNED_HEADERS {
        headers.remove(name);
    }
    headers
}

/// Merge a call's per-request wire headers over the transport's static headers (TD-0022 D2).
///
/// Transport-owned names (see [`TRANSPORT_OWNED_HEADERS`]) are stripped from the **per-call**
/// set so a library consumer can never override the vaulted credential or framing. Per-call
/// wins over static for every other name (single-valued: the FFI's string-map form cannot
/// express multi-value headers).
#[must_use]
pub fn merge_call_headers(base: &http::HeaderMap, call: &http::HeaderMap) -> http::HeaderMap {
    let mut out = base.clone();
    for (name, value) in call.iter() {
        if TRANSPORT_OWNED_HEADERS.contains(&name.as_str()) {
            continue; // transport-owned: the credential and framing are not caller-overridable
        }
        out.insert(name.clone(), value.clone());
    }
    out
}

/// A completed (non-streaming) response plus the usage measured **at the source**.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub status: u16,
    pub body: serde_json::Value,
    pub usage: ParsedUsage,
    /// Upstream attempts made for this logical call, including the successful attempt.
    pub attempts: u32,
}

/// One item of a streaming response: raw bytes to forward verbatim, plus the usage counts —
/// finalized on the terminal item, running on every item before it.
#[derive(Debug, Clone, Default)]
pub struct StreamChunk {
    /// Raw upstream bytes, forwarded to the caller unchanged (O(1) pass-through).
    pub data: Bytes,
    /// Present only on the terminal item: the finalized usage counts.
    ///
    /// `None` on the terminal item means the sniffer never matched anything — an upstream that
    /// ignored `stream_options.include_usage`, or a usage frame past [`MAX_STREAM_LINE_BYTES`]. That
    /// is deliberately distinct from `Some(ParsedUsage::default())`: a caller must be able to tell
    /// "the provider reported zero" from "the provider reported nothing", because settling the
    /// latter as a finalized zero silently discards a whole response's worth of spend (TD-0013).
    pub usage: Option<ParsedUsage>,
    /// The accumulator *so far*, `Some` once any line has actually moved it (TD-0013 D3).
    ///
    /// Anthropic reports input and the cache split on `message_start`, before a single content
    /// byte; Gemini attaches `usageMetadata` to chunks. Surfacing the running total is what lets an
    /// interrupted stream settle those real per-category numbers instead of a byte estimate. For a
    /// family that only reports at the end this stays `None` for the whole stream, so no caller
    /// has to know which family it is talking to — the absence of a number *is* the signal.
    pub usage_running: Option<ParsedUsage>,
    /// Upstream stream-setup attempts made for this logical call.
    pub attempts: u32,
}

/// A streaming response: a stream of [`StreamChunk`]s ending with a usage-bearing terminal item.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ProviderError>> + Send>>;

#[derive(Debug)]
#[non_exhaustive]
pub enum ProviderError {
    /// The caller supplied a malformed provider-native request. Never retry.
    InvalidRequest(String),
    /// 401 / 403 — bad or missing credential.
    Auth,
    /// 429 — provider rate limit.
    RateLimited,
    /// Any other non-success status, with a bounded snippet of the upstream
    /// response body when one was readable. 4xx bodies explain WHY the
    /// provider rejected the request (invalid tool pairing, unknown param,
    /// context overflow); dropping them forces consumers to debug blind.
    Upstream {
        status: u16,
        body: Option<String>,
        request_id: Option<String>,
    },
    /// Network / TLS / decode failure before or during the response.
    Transport(String),
    /// The circuit breaker is open (upstream failing) — the call was not attempted.
    CircuitOpen,
    /// The call (or stream setup / idle gap) exceeded the configured bound. Carries the bound
    /// for a self-describing message. Retryable — a timeout is a transient bet, like a 503.
    Timeout(std::time::Duration),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::InvalidRequest(e) => write!(f, "invalid request: {e}"),
            ProviderError::Auth => write!(f, "auth failed (401/403)"),
            ProviderError::RateLimited => write!(f, "rate limited (429)"),
            ProviderError::Upstream {
                status,
                body,
                request_id,
            } => {
                match body {
                    Some(body) => {
                        // Single-line, display-bounded snippet; the full (capped) body
                        // travels in ProviderErrorV1.details["upstream_body"].
                        let snippet: String = body
                            .chars()
                            .take(200)
                            .map(|c| if c == '\n' { ' ' } else { c })
                            .collect();
                        write!(f, "upstream status {status}: {snippet}")?;
                    }
                    None => write!(f, "upstream status {status}")?,
                }
                if let Some(id) = request_id {
                    write!(f, " [request-id: {id}]")?;
                }
                Ok(())
            }
            ProviderError::Transport(e) => write!(f, "transport error: {e}"),
            ProviderError::CircuitOpen => write!(f, "circuit open (upstream failing)"),
            ProviderError::Timeout(d) => write!(f, "timed out after {}s", d.as_secs_f32()),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Cap for captured upstream error bodies — diagnosability without unbounded memory.
pub(crate) const UPSTREAM_ERROR_BODY_CAP: usize = 2048;

/// Map a non-success HTTP status to a [`ProviderError`], carrying an optional
/// bounded snippet of the upstream response body.
pub(crate) fn error_for_status_with_body(
    status: u16,
    body: Option<String>,
    request_id: Option<String>,
) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth,
        429 => ProviderError::RateLimited,
        s => ProviderError::Upstream {
            status: s,
            body,
            request_id,
        },
    }
}

/// Consume a non-success HTTP response into a [`ProviderError`], capturing a
/// bounded snippet of the upstream body. Every adapter error path goes through
/// here so provider-rejection diagnostics reach consumers uniformly.
pub(crate) async fn error_for_response(
    resp: reqwest::Response,
    vendor_request_id_header: Option<&str>,
) -> ProviderError {
    let status = resp.status().as_u16();
    // The upstream request id (support-ticket currency). The shared path knows
    // only the de-facto standard names; a vendor that deviates declares its
    // header as a transport fact (`OpenAiCompatProviderSpec::request_id_header`,
    // e.g. Moonshot's `Msh-Request-Id`) or passes it from its adapter — vendor
    // differences are data/strategy, never branches in shared code.
    let request_id = ["x-request-id", "request-id"]
        .iter()
        .copied()
        .chain(vendor_request_id_header)
        .find_map(|header| resp.headers().get(header))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match resp.text().await {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.chars().take(UPSTREAM_ERROR_BODY_CAP).collect())
            }
        }
        Err(_) => None,
    };
    error_for_status_with_body(status, body, request_id)
}

/// The adapter contract every provider implements. The metering/resilience **decorator** will
/// wrap this so accounting + circuit-breaker + retry apply uniformly (a later milestone).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Neutral provider slug (e.g. `anthropic`, `openai`).
    fn slug(&self) -> &str;

    /// Non-streaming call. Extracts the full usage breakdown (incl. the cache split) from the
    /// provider's real response — never estimated.
    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError>;

    /// Streaming call: SSE pass-through (O(1) memory), usage finalized from the terminal block.
    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError>;
}

/// Hard cap on a single newline-delimited line while a stream is being decoded, on **both**
/// planes (TD-0014 P1).
///
/// This started as a 64 KiB "sniff budget" on the raw plane and adversarial review showed 64 KiB
/// was wrong on **both** planes for the same reason. OpenAI's Responses API puts the entire final
/// response object — all generated output included — in the single `response.completed` SSE line,
/// and that is also the line carrying the usage; a long generation is comfortably past 128 KiB.
/// Gemini's `inlineData` parts are larger still. A budget real traffic reaches is not a guard, it
/// is a bug: on the typed plane it killed working streams, and on the raw plane it silently
/// dropped the only usage frame the stream would ever send, settling the lease on a byte estimate.
///
/// One size, because the failure is the same size on both planes. **Two policies**, because the
/// consequence differs and that is decided at the call site, not here:
///
/// - **Raw plane** — drop the pending line and keep streaming. Bytes were already forwarded
///   verbatim, so only usage accuracy is at risk, and only for a line this large.
/// - **Typed decoders** — raise `ProviderError::Transport`. They emit decoded *content*, so
///   dropping silently would corrupt the response with no signal at all.
///
/// 8 MiB sits above every frame we can name and far below unbounded. It is a ceiling, not an
/// allocation: normal SSE carries newlines every few KB, so the buffer stays small. Worst-case
/// memory is this plus one chunk, times the concurrent streams — see TD-0014 P2, which bounds
/// the second factor. Sizing against a real corpus is TD-0015's job.
pub(crate) const MAX_STREAM_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Wrap a provider's byte stream in the metered pass-through: forward every upstream chunk
/// verbatim (O(1) forwarding, ADR-0047 D9) while running `sniff` over each complete newline-
/// delimited line to accumulate usage; the terminal item carries the finalized usage.
///
/// **Improvements over the original O(n²) / unbounded implementation (TD-0006):**
///
/// - **Bounded line buffer** — a single line exceeding [`MAX_STREAM_LINE_BYTES`] is flushed without
///   sniffing so memory stays bounded. A single-JSON-array stream (Gemini non-`?alt=sse`) cannot
///   exhaust memory. The bound is deliberately far above any real frame: see the constant.
/// - **O(n) scan** — tracks the last-scanned position so only newly-arrived bytes are searched
///   for `\n` on each chunk. The original rescanned the entire accumulated buffer on every chunk
///   (O(chunks²)).
/// - **`contains("usage")` guard** — skips the JSON parse inside `sniff` for lines that cannot
///   carry a usage object. Every known provider embeds the substring `"usage"` in usage-bearing
///   events (OpenAI/Anthropic/Cohere: `"usage"`; Gemini: `"usageMetadata"`). Non-usage lines
///   are forwarded without the parse overhead.
/// - **Transport-shape-aware final flush** — on stream end, sniffs any remaining buffered bytes.
///   This handles NDJSON without a trailing newline and, critically, the single-JSON-array
///   transport (Gemini's non-SSE stream: one `[{…},{…}]` with no line boundaries). If the
///   remaining buffer is within the budget, the sniff closure gets one final shot at extraction;
///   otherwise usage degrades gracefully to the default (zero) rather than blowing memory.
///
/// `sniff(line, &mut usage)` updates the running accumulator (SSE `data:` lines, Anthropic
/// events, NDJSON, or the terminal JSON array — the per-adapter parser decides).
pub(crate) fn metered_passthrough<S>(
    mut upstream: S,
    mut sniff: impl FnMut(&[u8], &mut ParsedUsage) -> bool + Send + 'static,
) -> ByteStream
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + Unpin + 'static,
{
    use futures_util::StreamExt;
    let s = async_stream::try_stream! {
        // The shared bounded/O(n) splitter (TD-0014 P1). The cursor and budget discipline this
        // function pioneered now lives in one place, so the typed decoders cannot drift from it.
        let mut splitter = crate::linesplit::LineSplitter::new(MAX_STREAM_LINE_BYTES);
        let mut usage = ParsedUsage::default();
        // Has any line moved the accumulator? Distinguishes "reported zero" from "reported
        // nothing" on the terminal item, and gates `usage_running` before it.
        let mut sniffed = false;
        while let Some(chunk) = upstream.next().await {
            let chunk = chunk.map_err(|e| ProviderError::Transport(e.to_string()))?;
            splitter.push(&chunk);
            while let Some(line) = splitter.next_line() {
                // Guard: skip the JSON parse for lines that can't carry a usage object.
                if line_contains_usage(&line) {
                    // The sniffer reports whether it recorded anything — comparing the accumulator
                    // before and after would be cheaper but wrong: a provider that legitimately
                    // reports all zeros would be indistinguishable from one that reported nothing,
                    // and those two must settle differently (TD-0013 D3).
                    sniffed |= sniff(&line, &mut usage);
                }
            }
            // Bounded memory: if no newline was found and the buffer exceeds the sniff budget,
            // the current line is too large (a giant tool-call delta, or a single-JSON-array
            // without the `?alt=sse` flag). Flush it without sniffing so memory stays bounded.
            // The bytes were already forwarded verbatim via `chunk` above.
            if splitter.over_budget() {
                splitter.reset();
            }
            yield StreamChunk {
                data: chunk,
                usage: None,
                usage_running: sniffed.then_some(usage),
                attempts: 1,
            };
        }
        // Transport-shape awareness: on stream end, sniff any remaining buffered bytes. Handles
        // NDJSON without a trailing newline and the single-JSON-array transport (Gemini's
        // non-`?alt=sse` stream). If within the budget, the sniff closure gets one final shot;
        // otherwise we degrade gracefully (default zero usage) rather than blowing memory.
        let remainder = splitter.remainder();
        if !remainder.is_empty()
            && remainder.len() <= MAX_STREAM_LINE_BYTES
            && line_contains_usage(remainder)
        {
            sniffed |= sniff(remainder, &mut usage);
        }
        // `None` when nothing was ever sniffed. Previously this yielded `Some(default())`, an
        // all-zero *finalized* usage that overwrote whatever the caller had accrued — so a stream
        // whose usage frame was never matched settled 0 after a full response (TD-0013 P1).
        yield StreamChunk {
            data: Bytes::new(),
            usage: sniffed.then_some(usage),
            usage_running: sniffed.then_some(usage),
            attempts: 1,
        };
    };
    Box::pin(s)
}

/// Check whether a byte slice could carry a usage object — a cheap pre-filter that avoids the
/// JSON parse for lines that cannot. Every known provider embeds one of these substrings in
/// usage-bearing events:
/// - OpenAI / Anthropic / Cohere: `"usage"`
/// - Gemini: `"usageMetadata"`
/// - Ollama: `"eval_count"` (`prompt_eval_count` / `eval_count`)
fn line_contains_usage(line: &[u8]) -> bool {
    contains_substring(line, b"usage") || contains_substring(line, b"eval_count")
}

fn contains_substring(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Extract the JSON payload from an SSE `data: {...}` line (skipping `[DONE]`), for the
/// per-adapter sniffers.
pub(crate) fn sse_data_json(line: &[u8]) -> Option<serde_json::Value> {
    let s = std::str::from_utf8(line).ok()?.trim();
    let payload = s.strip_prefix("data:")?.trim();
    if payload == "[DONE]" {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Test helper: drive `chunks` (a pre-split byte stream) through the production streaming
/// primitive (`metered_passthrough` + the adapter's real `sniff`) and return the finalized usage
/// from the terminal item. Shared by the per-provider chunk-boundary / forward-compat property
/// tests (TD-0001 W1) so each exercises the exact production path.
#[cfg(test)]
pub(crate) async fn accumulate_usage(
    chunks: Vec<Bytes>,
    sniff: impl FnMut(&[u8], &mut ParsedUsage) -> bool + Send + 'static,
) -> ParsedUsage {
    use futures_util::StreamExt;
    let upstream = futures_util::stream::iter(
        chunks
            .into_iter()
            .map(Ok::<Bytes, reqwest::Error>)
            .collect::<Vec<_>>(),
    );
    let mut out = metered_passthrough(Box::pin(upstream), sniff);
    let mut final_usage = None;
    while let Some(item) = out.next().await {
        let c = item.unwrap();
        if c.usage.is_some() {
            final_usage = c.usage;
        }
    }
    // The terminal item now carries `None` when the sniffer never matched (TD-0013): callers of
    // this helper always pass usage-bearing fixtures, so a `None` here means the fixture or the
    // sniffer regressed, not that the stream legitimately reported nothing.
    final_usage.expect("terminal item carries usage — the fixture is usage-bearing")
}

#[cfg(test)]
mod metered_passthrough_tests {
    use super::*;
    use bytes::Bytes;

    /// TD-0014 P1 regression, RAW plane (found by adversarial review of the typed fix).
    ///
    /// The typed ceiling was raised because OpenAI Responses puts the entire final response —
    /// **and the usage** — in the single `response.completed` SSE line, which a long generation
    /// pushes past 128 KiB. That reasoning applies identically here: this is the DEFAULT plane for
    /// a same-family call (`/v1/responses` -> OpenAI), and dropping the over-budget line loses the
    /// only usage frame the stream will ever send. The lease then settles on a byte estimate, so
    /// spend is silently undercounted for any long generation.
    ///
    /// Dropping is still the right *policy* here (bytes are already forwarded, so content is
    /// unharmed) — the budget was simply too small to ever reach the frame that matters.
    #[tokio::test]
    async fn a_long_terminal_frame_still_yields_usage_on_the_raw_plane() {
        let long_text = "word ".repeat(26_000);
        let wire = format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}}\n\n\
             data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\
             \"output\":[{{\"type\":\"message\",\"text\":\"{long_text}\"}}],\
             \"usage\":{{\"input_tokens\":5,\"output_tokens\":32000}}}}}}\n\n"
        );
        assert!(
            wire.len() > 64 * 1024,
            "fixture must exceed the old 64 KiB budget"
        );
        // Chunked the way a socket delivers it — a single push would drain the complete line
        // before the budget is ever consulted, which is how this class of bug hides.
        let chunks: Vec<Bytes> = wire
            .as_bytes()
            .chunks(16 * 1024)
            .map(Bytes::copy_from_slice)
            .collect();
        let usage =
            accumulate_usage(chunks, crate::openai_responses::sniff_responses_usage_line).await;
        assert_eq!(
            usage.tokens_out, 32_000,
            "the terminal usage frame must survive"
        );
        assert_eq!(usage.tokens_in, 5);
    }
    use futures_util::StreamExt;

    /// Drive chunks through `metered_passthrough` and return (finalized usage, all forwarded data).
    async fn drive(
        chunks: Vec<Bytes>,
        sniff: impl FnMut(&[u8], &mut ParsedUsage) -> bool + Send + 'static,
    ) -> (ParsedUsage, Vec<u8>) {
        let upstream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(Ok::<Bytes, reqwest::Error>)
                .collect::<Vec<_>>(),
        );
        let mut out = metered_passthrough(Box::pin(upstream), sniff);
        let mut final_usage = ParsedUsage::default();
        let mut forwarded = Vec::new();
        while let Some(item) = out.next().await {
            let c = item.unwrap();
            forwarded.extend_from_slice(&c.data);
            if let Some(u) = c.usage {
                final_usage = u;
            }
        }
        (final_usage, forwarded)
    }

    /// Bounded-sniffer: a single-JSON-array (Gemini non-`?alt=sse` shape) with no line
    /// boundaries must not blow the buffer. The whole response accumulates in the line buffer;
    /// on stream end the final flush sniffs it (within budget) and usage is extracted.
    #[tokio::test]
    async fn single_json_array_within_budget_extracts_usage_on_final_flush() {
        // Gemini non-SSE stream shape: one JSON array, no newlines.
        let array = br#"[{"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"cachedContentTokenCount":3}}]"#;
        let chunks: Vec<Bytes> = array.chunks(16).map(Bytes::copy_from_slice).collect();

        // A sniffer that understands non-SSE JSON (not the SSE-specific sse_data_json).
        fn sniff(line: &[u8], usage: &mut ParsedUsage) -> bool {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                if let Some(arr) = v.as_array() {
                    // Gemini puts usageMetadata on the last array element.
                    if let Some(last) = arr.last() {
                        if let Some(meta) = last.get("usageMetadata") {
                            usage.tokens_in = meta
                                .get("promptTokenCount")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0)
                                - meta
                                    .get("cachedContentTokenCount")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0);
                            usage.tokens_out = meta
                                .get("candidatesTokenCount")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            usage.cache_read_tokens = meta
                                .get("cachedContentTokenCount")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            return true;
                        }
                    }
                }
            }
            false
        }

        let (usage, forwarded) = drive(chunks, sniff).await;
        // All bytes forwarded verbatim.
        assert_eq!(forwarded, array.to_vec());
        // Usage extracted from the final flush (the array has no line boundaries).
        assert_eq!(usage.tokens_in, 7); // 10 - 3 cached
        assert_eq!(usage.tokens_out, 5);
        assert_eq!(usage.cache_read_tokens, 3);
    }

    /// Bounded-sniffer: a single SSE line exceeding MAX_STREAM_LINE_BYTES must not blow the buffer.
    /// The line is flushed (forwarded) without sniffing — usage degrades gracefully to default.
    #[tokio::test]
    async fn huge_single_sse_line_does_not_blow_buffer() {
        // Build a single SSE line far exceeding MAX_STREAM_LINE_BYTES (64 KiB).
        let padding = "x".repeat(MAX_STREAM_LINE_BYTES + 10_000);
        let sse_line = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{padding}\"}}}}],\"usage\":{{\"prompt_tokens\":42,\"completion_tokens\":7}}}}\n"
        );
        let body = sse_line.as_bytes().to_vec();
        // Feed as small chunks to exercise the bounded-buffer path.
        let chunks: Vec<Bytes> = body.chunks(1024).map(Bytes::copy_from_slice).collect();

        fn sniff(line: &[u8], usage: &mut ParsedUsage) -> bool {
            if let Some(v) = sse_data_json(line) {
                if v.get("usage").is_some_and(|u| !u.is_null()) {
                    if let Some(u) = parse_openai_usage(&v) {
                        *usage = u;
                        return true;
                    }
                }
            }
            false
        }

        let (usage, forwarded) = drive(chunks, sniff).await;
        // All bytes forwarded verbatim — no data loss.
        assert_eq!(forwarded, body);
        // Usage gracefully defaults to zero (the oversize line was flushed without sniffing).
        assert_eq!(usage, ParsedUsage::default());
    }

    /// The `contains("usage")` guard correctly identifies usage-bearing lines and skips others.
    #[test]
    fn line_contains_usage_guard() {
        assert!(line_contains_usage(
            b"data: {\"usage\":{\"prompt_tokens\":10}}\n"
        ));
        assert!(line_contains_usage(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":10}}\n"
        ));
        assert!(!line_contains_usage(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n"
        ));
        assert!(!line_contains_usage(b"event: ping\n"));
        assert!(!line_contains_usage(b"\n"));
    }

    /// Transport-shape awareness: a single JSON object without a trailing newline (NDJSON's
    /// final line, or a non-streaming body forwarded through the stream path) still extracts
    /// usage via the final flush.
    #[tokio::test]
    async fn no_trailing_newline_extracts_usage_on_final_flush() {
        // One NDJSON line with usage — no trailing newline so it never hits the line-boundary
        // path; it stays in line_buf until the final flush.
        let body = br#"{"id":"b","usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        let chunks: Vec<Bytes> = vec![Bytes::copy_from_slice(body)];

        fn sniff(line: &[u8], usage: &mut ParsedUsage) -> bool {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                if v.get("usage").is_some_and(|u| !u.is_null()) {
                    if let Some(u) = parse_openai_usage(&v) {
                        *usage = u;
                        return true;
                    }
                }
            }
            false
        }

        let (usage, _) = drive(chunks, sniff).await;
        assert_eq!(usage.tokens_in, 10);
        assert_eq!(usage.tokens_out, 5);
    }

    /// Normal SSE still works: chunk-boundary invariance preserved (the improvement doesn't
    /// regress the existing property tested by per-adapter suites).
    #[tokio::test]
    async fn normal_sse_still_extracts_usage_across_chunk_boundaries() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes();
        // Split at every offset — usage must be invariant.
        fn sniff(line: &[u8], usage: &mut ParsedUsage) -> bool {
            if let Some(v) = sse_data_json(line) {
                if v.get("usage").is_some_and(|u| !u.is_null()) {
                    if let Some(u) = parse_openai_usage(&v) {
                        *usage = u;
                        return true;
                    }
                }
            }
            false
        }
        for split in 0..=sse.len() {
            let chunks = vec![
                Bytes::copy_from_slice(&sse[..split]),
                Bytes::copy_from_slice(&sse[split..]),
            ];
            let (usage, _) = drive(chunks, sniff).await;
            assert_eq!(usage.tokens_in, 10, "split {split}");
            assert_eq!(usage.tokens_out, 5, "split {split}");
        }
    }
}

#[cfg(test)]
mod error_for_response_tests {
    use super::*;

    fn response_with(status: u16, headers: &[(&str, &str)], body: &str) -> reqwest::Response {
        let mut builder = http::Response::builder().status(status);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        reqwest::Response::from(builder.body(body.to_owned()).expect("test response"))
    }

    #[tokio::test]
    async fn vendor_declared_header_is_extracted() {
        // The vendor header arrives as DATA (spec/adapter), not a shared-list entry.
        let resp = response_with(
            404,
            &[("Msh-Request-Id", "msh_abc123")],
            r#"{"error":{"message":"Not found the model"}}"#,
        );
        match error_for_response(resp, Some("msh-request-id")).await {
            ProviderError::Upstream {
                status,
                body,
                request_id,
            } => {
                assert_eq!(status, 404);
                assert_eq!(request_id.as_deref(), Some("msh_abc123"));
                assert!(body.unwrap().contains("Not found the model"));
            }
            other => panic!("expected Upstream, got {other}"),
        }
    }

    #[tokio::test]
    async fn standard_header_wins_over_vendor_and_edge() {
        let resp = response_with(
            500,
            &[("x-request-id", "std-1"), ("Msh-Request-Id", "msh-1")],
            "",
        );
        match error_for_response(resp, Some("msh-request-id")).await {
            ProviderError::Upstream { request_id, .. } => {
                assert_eq!(request_id.as_deref(), Some("std-1"));
            }
            other => panic!("expected Upstream, got {other}"),
        }
    }

    #[tokio::test]
    async fn infrastructure_headers_are_not_request_ids() {
        // cf-ray is an edge trace id, not the provider's request id — the field's
        // semantics stay clean: no vendor declaration, no id.
        let resp = response_with(502, &[("cf-ray", "8f3abc-SJC")], "bad gateway");
        match error_for_response(resp, None).await {
            ProviderError::Upstream { request_id, .. } => {
                assert_eq!(request_id, None);
            }
            other => panic!("expected Upstream, got {other}"),
        }
    }

    #[test]
    fn moonshot_spec_declares_its_request_id_header() {
        let spec = resolve_openai_compat_provider("moonshot").expect("moonshot spec");
        assert_eq!(spec.request_id_header, Some("msh-request-id"));
    }
}

#[cfg(test)]
mod streaming_usage_fidelity_tests {
    //! TD-0013 — the running accumulator, and the difference between "reported zero" and
    //! "reported nothing".

    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;

    /// Every item a stream produced, so a test can assert on what was visible *before* the end.
    ///
    /// The body is fed **one SSE frame per chunk**, because that is the only way the ordering
    /// question ("was this known before the end?") is meaningful — delivered as a single chunk,
    /// every frame including the terminal one arrives at once and every family looks incremental.
    async fn collect_chunks(
        body: &[u8],
        sniff: impl FnMut(&[u8], &mut ParsedUsage) -> bool + Send + 'static,
    ) -> Vec<StreamChunk> {
        let frames: Vec<Bytes> = String::from_utf8_lossy(body)
            .split_inclusive("\n\n")
            .map(|frame| Bytes::copy_from_slice(frame.as_bytes()))
            .collect();
        let upstream = futures_util::stream::iter(
            frames
                .into_iter()
                .map(Ok::<Bytes, reqwest::Error>)
                .collect::<Vec<_>>(),
        );
        let mut out = metered_passthrough(Box::pin(upstream), sniff);
        let mut chunks = Vec::new();
        while let Some(item) = out.next().await {
            chunks.push(item.unwrap());
        }
        chunks
    }

    /// The whole point. Anthropic announces input and the cache split on `message_start`, so a
    /// consumer must be able to see those counts *before* the terminal item — that is what lets an
    /// interrupted stream settle a real number instead of a byte estimate.
    #[tokio::test]
    async fn an_incremental_family_exposes_usage_before_the_terminal_item() {
        let body: &[u8] = include_bytes!("../tests/fixtures/anthropic/stream_cache_split.sse");
        let chunks = collect_chunks(body, crate::anthropic::sniff_usage_line).await;

        // The very first frame is `message_start`, and it is already enough to bill the prompt.
        let running = chunks[0]
            .usage_running
            .expect("Anthropic announces input and the cache split on the first frame");

        assert_eq!(running.tokens_in, 1024, "input is known at message_start");
        assert_eq!(running.cache_creation_tokens, 2048);
        assert_eq!(running.cache_read_tokens, 4096);

        // And the terminal item still carries the finalized counts — progress must not replace
        // the verdict.
        assert_eq!(
            chunks
                .last()
                .unwrap()
                .usage
                .expect("terminal usage")
                .tokens_out,
            256,
            "the terminal item still carries the finalized output count"
        );
    }

    /// The converse, and the reason no caller needs to know which family it is talking to: a
    /// terminal-only family has told us nothing at the moment a disconnect would happen, so the
    /// absence of a number is itself the signal to fall back to the estimate.
    ///
    /// The question is specifically "what was known while content was still streaming" — asserting
    /// over *all* pre-terminal chunks would be wrong, since the frame carrying OpenAI's usage is
    /// itself a data chunk.
    #[tokio::test]
    async fn a_terminal_only_family_exposes_nothing_while_content_streams() {
        let body: &[u8] = include_bytes!("../tests/fixtures/openai/stream.sse");
        let chunks = collect_chunks(body, crate::openai::sniff_usage_line).await;

        // Note OpenAI puts `"usage": null` on *every* streamed chunk, which is precisely why the
        // sniffer's null guard exists: a usage-*shaped* frame is not a usage report.
        let first_exposed = chunks
            .iter()
            .position(|chunk| chunk.usage_running.is_some())
            .expect("the fixture does report usage, eventually");
        assert!(
            first_exposed > 0,
            "nothing is known when the stream begins — a disconnect here has no number to settle"
        );
        assert!(
            String::from_utf8_lossy(&chunks[first_exposed].data).contains("completion_tokens"),
            "usage must not surface before the frame that actually carries it; otherwise the proxy \
             would settle a number the provider never sent"
        );
        assert!(
            chunks.last().unwrap().usage.is_some(),
            "the terminal item must still carry the finalized usage"
        );
    }

    /// The second integrity hole TD-0013 closes. A stream whose usage is never matched — an
    /// upstream that ignored `stream_options.include_usage`, or a usage frame past the sniff
    /// budget — used to yield a terminal `Some(ParsedUsage::default())`. That all-zero *finalized*
    /// usage overwrote whatever the caller had accrued, so a full response settled `0`.
    #[tokio::test]
    async fn a_stream_that_reports_nothing_is_distinguishable_from_one_reporting_zero() {
        let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n";
        let chunks = collect_chunks(body, crate::openai::sniff_usage_line).await;

        let terminal = chunks.last().expect("a terminal item is always produced");
        assert!(
            terminal.usage.is_none(),
            "a stream the sniffer never matched must not present an authoritative zero — the \
             caller has to be able to keep its own accrued estimate"
        );
        assert!(terminal.usage_running.is_none());

        // The bytes are still forwarded verbatim regardless: metering must never cost fidelity.
        let forwarded: Vec<u8> = chunks.iter().flat_map(|c| c.data.to_vec()).collect();
        assert_eq!(forwarded, body);
    }

    /// A provider that genuinely reports zeros is a measurement, and must survive as one.
    #[tokio::test]
    async fn a_genuine_zero_is_still_reported() {
        let body =
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\n\n";
        let chunks = collect_chunks(body, crate::openai::sniff_usage_line).await;

        assert_eq!(
            chunks.last().unwrap().usage,
            Some(ParsedUsage::default()),
            "reporting zero is not the same as reporting nothing"
        );
    }
}

#[cfg(test)]
mod usage_cadence_conformance {
    //! TD-0013 D2 — the declared cadence, checked against what each family's shipped fixture
    //! actually does through the production `metered_passthrough` path.
    //!
    //! A transport fact nobody can refute is a comment, and comments drift. The property asserted
    //! is the operationally meaningful one: **at the moment the last content byte arrives, is there
    //! a number to settle?** That is exactly the question an interrupted stream asks.

    use super::*;
    use crate::typed::{ProviderFamily, UsageCadence};
    use bytes::Bytes;
    use futures_util::StreamExt;

    struct FamilyCase {
        family: ProviderFamily,
        fixture: &'static [u8],
        sniff: fn(&[u8], &mut ParsedUsage) -> bool,
        /// Substring identifying a line that delivers model output, as opposed to a trailing
        /// control frame. What separates the two cadences is whether usage has arrived by the
        /// time the last of these has.
        content_marker: &'static str,
    }

    fn cases() -> Vec<FamilyCase> {
        vec![
            FamilyCase {
                family: ProviderFamily::Anthropic,
                fixture: include_bytes!("../tests/fixtures/anthropic/stream_cache_split.sse"),
                sniff: crate::anthropic::sniff_usage_line,
                content_marker: "content_block_delta",
            },
            FamilyCase {
                family: ProviderFamily::Gemini,
                fixture: include_bytes!("../tests/fixtures/gemini/stream.sse"),
                sniff: crate::gemini::sniff_usage_line,
                content_marker: "\"text\"",
            },
            FamilyCase {
                family: ProviderFamily::OpenAiCompat,
                fixture: include_bytes!("../tests/fixtures/openai/stream.sse"),
                sniff: crate::openai::sniff_usage_line,
                content_marker: "\"delta\"",
            },
            FamilyCase {
                family: ProviderFamily::Cohere,
                fixture: include_bytes!("../tests/fixtures/cohere/stream.sse"),
                sniff: crate::cohere::sniff_usage_line,
                content_marker: "content-delta",
            },
            FamilyCase {
                family: ProviderFamily::Ollama,
                fixture: include_bytes!("../tests/fixtures/ollama/stream.ndjson"),
                sniff: crate::local::sniff_usage_line,
                content_marker: "\"done\":false",
            },
        ]
    }

    /// Feed `lines` through the real primitive and report whether any pre-terminal item exposed a
    /// running total.
    async fn usage_known_after(
        lines: Vec<Bytes>,
        sniff: fn(&[u8], &mut ParsedUsage) -> bool,
    ) -> bool {
        let upstream = futures_util::stream::iter(
            lines
                .into_iter()
                .map(Ok::<Bytes, reqwest::Error>)
                .collect::<Vec<_>>(),
        );
        let mut out = metered_passthrough(Box::pin(upstream), sniff);
        let mut known = false;
        while let Some(item) = out.next().await {
            let chunk = item.unwrap();
            // Only pre-terminal items count: the terminal item is synthesized by the primitive
            // itself and is exactly what an interrupted stream never receives.
            if !chunk.data.is_empty() && chunk.usage_running.is_some() {
                known = true;
            }
        }
        known
    }

    #[tokio::test]
    async fn every_family_behaves_as_its_declaration_claims() {
        for case in cases() {
            let text = String::from_utf8_lossy(case.fixture).into_owned();
            let lines: Vec<&str> = text.split_inclusive('\n').collect();
            let last_content = lines
                .iter()
                .rposition(|line| line.contains(case.content_marker))
                .unwrap_or_else(|| {
                    panic!(
                        "{:?}: the fixture has no line matching {:?}, so this proves nothing",
                        case.family, case.content_marker
                    )
                });

            // Everything the client would have received by the time the last content byte landed.
            let delivered: Vec<Bytes> = lines[..=last_content]
                .iter()
                .map(|line| Bytes::copy_from_slice(line.as_bytes()))
                .collect();
            let known = usage_known_after(delivered, case.sniff).await;

            match case.family.usage_cadence() {
                UsageCadence::Incremental => assert!(
                    known,
                    "{:?} is declared Incremental but reported nothing by the end of its content \
                     — either the declaration is wrong or the sniffer regressed, and an \
                     interrupted stream would silently fall back to a byte estimate",
                    case.family
                ),
                UsageCadence::TerminalOnly => assert!(
                    !known,
                    "{:?} is declared TerminalOnly but exposed usage while content was still \
                     arriving — the declaration understates the family, and the fallback is \
                     being applied where a real measurement exists",
                    case.family
                ),
            }
        }
    }

    /// The declaration must be exhaustive: a family added without a considered cadence would
    /// otherwise inherit whatever the catch-all arm happens to say.
    #[test]
    fn every_family_declares_a_cadence() {
        for family in [
            ProviderFamily::OpenAiCompat,
            ProviderFamily::OpenAiResponses,
            ProviderFamily::Anthropic,
            ProviderFamily::Cohere,
            ProviderFamily::Gemini,
            ProviderFamily::Ollama,
        ] {
            let _ = family.usage_cadence();
        }
    }
}
