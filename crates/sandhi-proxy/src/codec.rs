//! HTTP ingress/egress codecs for the typed gateway front door.
//!
//! These codecs project public dialects into the canonical chat contract. Decoder-owned
//! native input remains in its family bucket for compatible, loss-aware re-encoding.

use std::collections::BTreeMap;

use axum::http::{HeaderMap, StatusCode};
use base64::Engine as _;
use sandhi_core::chat::ThinkingV1;
use sandhi_core::{
    ChatMessageV1, ChatRequestV1, ChatResponseV1, ChatStreamEventV1, ContentPart, FinishReasonV1,
    MessageContent, RequestMetadataV1, ToolCallV1, ToolChoiceMode, ToolChoiceV1, ToolDefinitionV1,
    UsageV2, CHAT_SCHEMA_VERSION_V1,
};
use serde_json::{json, Map, Value};
mod fidelity;
mod responses_stream;

/// Lift the OpenAI-compat / ZAI `thinking` object (`{type, budget_tokens}`)
/// into the neutral `ThinkingV1` at ingress (W3d/G7), so a promoted param
/// reaches the typed field instead of surviving only inside the extensions
/// body clone (which dies on any cross-family route).
fn lift_openai_thinking(object: &Map<String, Value>) -> Option<ThinkingV1> {
    let thinking = object.get("thinking")?.as_object()?;
    let enabled = match thinking.get("type").and_then(Value::as_str) {
        Some("disabled") => false,
        _ => true, // "enabled" or a bare budget object both mean on
    };
    Some(ThinkingV1 {
        enabled,
        budget_tokens: thinking.get("budget_tokens").and_then(Value::as_u64),
    })
}

/// Lift Gemini's `generationConfig.thinkingConfig.thinkingBudget` into the
/// neutral `ThinkingV1` (W3d/G7). Budget 0 = disabled; a positive budget = on
/// with a cap.
fn lift_gemini_thinking(object: &Map<String, Value>) -> Result<Option<ThinkingV1>, String> {
    let Some(config) = object
        .get("generationConfig")
        .and_then(|v| v.get("thinkingConfig"))
    else {
        return Ok(None);
    };
    if config.get("thinkingBudget").is_some() && config.get("thinking_budget").is_some() {
        return Err("Gemini thinking budget aliases must not both be supplied".into());
    }
    let Some(budget) = config
        .get("thinkingBudget")
        .or_else(|| config.get("thinking_budget"))
        .and_then(Value::as_u64)
    else {
        return Ok(None);
    };
    Ok(Some(ThinkingV1 {
        enabled: budget != 0,
        budget_tokens: (budget != 0).then_some(budget),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressDialect {
    OpenAi,
    /// Same-family raw transport only; canonical request is used for admission/accounting.
    Embeddings,
    Completions,
    Anthropic,
    /// OpenAI Responses item/event protocol (`/v1/responses`). Normalized through the same
    /// `ChatRequestV1` as the other dialects; the field mapping mirrors the typed Responses
    /// codec in `sandhi-providers::openai_responses_typed`.
    Responses,
    /// Google Gemini (`/v1beta/models/{model}:generateContent`). Structurally unlike the other
    /// three: the **model and the streaming choice live in the path**, not the body, and the
    /// credential is `x-goog-api-key`.
    ///
    /// TD-0010 D4a admits this dialect on the **transparent plane only** — a Gemini client must
    /// resolve to a Gemini upstream, where the body is forwarded byte-for-byte. Its decode below
    /// is therefore accounting-grade (enough to meter and enforce), NOT a faithful round-trip;
    /// cross-family translation is refused rather than served from a lossy decode (D4b).
    Gemini,
}

/// The single owner of client-facing error construction (TD-0021 P3, D6).
///
/// Status, machine code, dialect rendering, AND the TD-0008 D redaction decision live on
/// this type — the value is that the *next* error path cannot forget the redaction rule:
/// the default constructors strip upstream bodies and transport internals, and full detail
/// is an explicit, named choice (`from_provider_full`), never
/// a default. Behaviour-identical to the four construction sites it absorbs
/// (`ingress_error`, `provider_error`, the admin `error`/`err` pair stay as thin
/// delegates so callers and the dialect-shaping tests are unchanged).
pub(crate) struct IngressError {
    status: axum::http::StatusCode,
    code: String,
    message: String,
    /// The full typed payload (ProviderErrorV1). Present ONLY in full-detail mode;
    /// redacted mode carries the support-escalation currency (code/status/request_id)
    /// with `details` cleared and `message` replaced by the canonical short form.
    typed: Option<sandhi_core::ProviderErrorV1>,
}

impl IngressError {
    pub(crate) fn policy(
        status: axum::http::StatusCode,
        code: &str,
        receipt: Option<&str>,
    ) -> Self {
        let typed = sandhi_core::ProviderErrorV1 {
            code: code.into(),
            message: "Request stopped by gateway policy".into(),
            retryable: false,
            http_status: Some(status.as_u16()),
            provider: None,
            request_id: receipt.map(str::to_owned),
            details: Default::default(),
        };
        Self {
            status,
            code: code.into(),
            message: typed.message.clone(),
            typed: Some(typed),
        }
    }
    pub(crate) fn draining() -> Self {
        Self {
            status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
            code: "gateway_draining".into(),
            message: "gateway draining; use a ready instance".into(),
            typed: None,
        }
    }

    /// An operator/protocol refusal the proxy itself generated (bad key, malformed
    /// body, throttle). Never carries upstream bytes, so no redaction decision exists.
    pub(crate) fn invalid(status: axum::http::StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code: "invalid_request".into(),
            message: message.into(),
            typed: None,
        }
    }

    /// A provider-boundary failure, REDACTED by default (TD-0008 D): keep code /
    /// http_status / request_id, drop upstream bodies and transport internals. This is
    /// the constructor every new error path should reach for; the full-detail variant
    /// is the explicit opt-in.
    pub(crate) fn from_provider_redacted(
        e: &sandhi_providers::ProviderError,
        provider: &str,
    ) -> Self {
        Self::from_provider_detail(e, provider, false)
    }

    /// The `SANDHI_ERROR_DETAIL=full` opt-in (single-tenant / self-hosted): forwards
    /// the full ProviderErrorV1 including the bounded upstream body.
    pub(crate) fn from_provider_full(e: &sandhi_providers::ProviderError, provider: &str) -> Self {
        Self::from_provider_detail(e, provider, true)
    }

    fn from_provider_detail(
        e: &sandhi_providers::ProviderError,
        provider: &str,
        full_detail: bool,
    ) -> Self {
        let (status, canonical) = match e {
            sandhi_providers::ProviderError::InvalidRequest(_) => (
                axum::http::StatusCode::BAD_REQUEST,
                "invalid provider request",
            ),
            sandhi_providers::ProviderError::OutputProjection(_) => (
                axum::http::StatusCode::BAD_GATEWAY,
                "upstream output cannot be represented by the selected client protocol",
            ),
            sandhi_providers::ProviderError::Auth => {
                (axum::http::StatusCode::BAD_GATEWAY, "upstream auth failed")
            }
            sandhi_providers::ProviderError::RateLimited => (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "upstream rate limited",
            ),
            sandhi_providers::ProviderError::Upstream { .. } => {
                (axum::http::StatusCode::BAD_GATEWAY, "upstream error")
            }
            sandhi_providers::ProviderError::Transport(_) => (
                axum::http::StatusCode::BAD_GATEWAY,
                "upstream transport error",
            ),
            sandhi_providers::ProviderError::CircuitOpen => (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "circuit open (upstream failing)",
            ),
            sandhi_providers::ProviderError::Timeout(_) => (
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                "upstream timed out",
            ),
            // ProviderError is #[non_exhaustive]; unknown future variants degrade to 502.
            _ => (axum::http::StatusCode::BAD_GATEWAY, "upstream error"),
        };
        let typed = e.as_typed(Some(provider));
        let (message, typed) = if full_detail {
            (typed.message.clone(), typed)
        } else {
            let mut t = typed;
            t.message = canonical.to_owned();
            t.details.clear();
            (canonical.to_owned(), t)
        };
        Self {
            status,
            code: "upstream_error".into(),
            message,
            typed: Some(typed),
        }
    }

    /// Render into the ingress dialect's error envelope. One rendering site for every
    /// construction path — a future dialect is added here once, not per call site.
    pub(crate) fn render(&self, dialect: IngressDialect) -> axum::response::Response {
        let body = match dialect {
            IngressDialect::OpenAi
            | IngressDialect::Embeddings
            | IngressDialect::Completions
            | IngressDialect::Responses => {
                json!({"error": self.payload()})
            }
            IngressDialect::Anthropic => json!({"type":"error","error": self.payload()}),
            // Google's shape: `error.code` is the HTTP status as a NUMBER and `error.status`
            // is the canonical enum name — that is what a google-genai client reads.
            IngressDialect::Gemini => json!({"error":{
                "code": self.status.as_u16(),
                "message": self.message,
                "status": google_status(self.status),
            }}),
        };
        use axum::response::IntoResponse;
        let response = (self.status, axum::Json(body)).into_response();
        // Status-based SDK defaults otherwise retry gateway 502s even when the
        // typed body says this is a deterministic, nonretryable refusal.
        if self.typed.as_ref().is_some_and(|error| !error.retryable) {
            crate::settlement::buffered::no_automatic_retry(response)
        } else {
            response
        }
    }

    /// The dialect-neutral typed payload: the full ProviderErrorV1 when present, else the
    /// minimal `{code, message, retryable, http_status}` the protocol refusals always carried.
    fn payload(&self) -> Value {
        match &self.typed {
            Some(typed) => serde_json::to_value(typed).unwrap_or_else(|_| {
                json!({
                    "code": self.code,
                    "message": self.message,
                    "retryable": false,
                    "http_status": self.status.as_u16(),
                })
            }),
            None => json!({
                "code": self.code,
                "message": self.message,
                "retryable": self.code == "gateway_draining",
                "http_status": self.status.as_u16(),
            }),
        }
    }
}

/// HTTP status → the canonical google.rpc status name a Gemini client expects.
pub(crate) fn google_status(status: axum::http::StatusCode) -> &'static str {
    match status {
        axum::http::StatusCode::BAD_REQUEST => "INVALID_ARGUMENT",
        axum::http::StatusCode::UNAUTHORIZED => "UNAUTHENTICATED",
        axum::http::StatusCode::FORBIDDEN => "PERMISSION_DENIED",
        axum::http::StatusCode::NOT_FOUND => "NOT_FOUND",
        axum::http::StatusCode::TOO_MANY_REQUESTS => "RESOURCE_EXHAUSTED",
        axum::http::StatusCode::PAYLOAD_TOO_LARGE => "RESOURCE_EXHAUSTED",
        axum::http::StatusCode::NOT_IMPLEMENTED => "UNIMPLEMENTED",
        axum::http::StatusCode::SERVICE_UNAVAILABLE => "UNAVAILABLE",
        axum::http::StatusCode::GATEWAY_TIMEOUT => "DEADLINE_EXCEEDED",
        _ => "INTERNAL",
    }
}

/// One way a client may present its virtual key on the wire.
///
/// Kept as data rather than as branches in a handler so that a dialect declares its credential
/// contract in one place (TD-0010 D1) — adding a dialect means listing its schemes, not editing
/// the auth path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialScheme {
    /// `Authorization: Bearer <vk>` — the OpenAI-family presentation.
    AuthorizationBearer,
    /// A header whose entire value is the credential, e.g. Anthropic's `x-api-key: <vk>`.
    RawHeader(&'static str),
}

impl CredentialScheme {
    fn extract(self, headers: &HeaderMap) -> Option<&str> {
        let raw = match self {
            CredentialScheme::AuthorizationBearer => headers
                .get("authorization")?
                .to_str()
                .ok()?
                .strip_prefix("Bearer ")?,
            CredentialScheme::RawHeader(name) => headers.get(name)?.to_str().ok()?,
        };
        let token = raw.trim();
        (!token.is_empty()).then_some(token)
    }
}

impl IngressDialect {
    /// The credential presentations this dialect accepts, **most-native first**.
    ///
    /// A dialect is the whole client-facing contract, not just a body schema: a stock SDK
    /// authenticates the way its vendor documents. The Anthropic SDK sends `x-api-key`, so a
    /// gateway that only reads `Authorization: Bearer` 401s an unmodified client. `Bearer` is
    /// accepted everywhere as a secondary form — it costs nothing and it keeps curl users and
    /// header-stripping intermediaries working.
    fn credential_schemes(self) -> &'static [CredentialScheme] {
        match self {
            // No `x-api-key` here on purpose: the OpenAI SDKs never send it, so accepting it
            // would invent a cross-vendor auth scheme no client asked for.
            IngressDialect::OpenAi
            | IngressDialect::Embeddings
            | IngressDialect::Completions
            | IngressDialect::Responses => &[CredentialScheme::AuthorizationBearer],
            IngressDialect::Anthropic => &[
                CredentialScheme::RawHeader("x-api-key"),
                CredentialScheme::AuthorizationBearer,
            ],
            // Header only. Gemini also documents `?key=<credential>`, which puts a live virtual
            // key in a URL — access logs, crash reports, browser history. Until that can be
            // guaranteed not to be logged end to end, the query form is a stated gap in the
            // compatibility matrix rather than a quiet acceptance (TD-0010 D4a).
            IngressDialect::Gemini => &[
                CredentialScheme::RawHeader("x-goog-api-key"),
                CredentialScheme::AuthorizationBearer,
            ],
        }
    }

    /// How this dialect's own SDK is documented to present a credential — used in the 401 so the
    /// message tells a client what its vendor actually sends, not what another vendor sends.
    pub(crate) fn credential_hint(self) -> &'static str {
        match self {
            IngressDialect::OpenAi
            | IngressDialect::Embeddings
            | IngressDialect::Completions
            | IngressDialect::Responses => "'Authorization: Bearer <key>'",
            IngressDialect::Anthropic => "'x-api-key: <key>' (or 'Authorization: Bearer <key>')",
            IngressDialect::Gemini => "'x-goog-api-key: <key>' (or 'Authorization: Bearer <key>')",
        }
    }

    /// The client's virtual key, read in whichever presentation this dialect accepts.
    /// The dialect-native scheme wins when a request carries more than one.
    pub(crate) fn extract_credential(self, headers: &HeaderMap) -> Option<&str> {
        self.credential_schemes()
            .iter()
            .find_map(|scheme| scheme.extract(headers))
    }
}

pub(crate) fn decode_request(
    dialect: IngressDialect,
    mut body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    let cache = body
        .as_object_mut()
        .and_then(|object| object.remove("sandhi_cache"))
        .map(serde_json::from_value::<sandhi_core::CacheIntentV1>)
        .transpose()
        .map_err(|_| "sandhi_cache must be a valid cache intent object".to_string())?;
    if cache.is_some()
        && matches!(
            dialect,
            IngressDialect::Embeddings | IngressDialect::Completions
        )
    {
        return Err("sandhi_cache is supported only on chat endpoints".into());
    }
    if let Some(intent) = &cache {
        intent.validate().map_err(str::to_owned)?;
    }
    let (mut request, stream) = match dialect {
        IngressDialect::OpenAi => decode_openai_request(body, metadata),
        IngressDialect::Embeddings | IngressDialect::Completions => {
            crate::text_endpoints::decode(dialect, body, metadata)
        }
        IngressDialect::Anthropic => decode_anthropic_request(body, metadata),
        IngressDialect::Responses => decode_responses_request(body, metadata),
        IngressDialect::Gemini => decode_gemini_request(body, metadata),
    }?;
    request.cache_intent = cache;
    Ok((request, stream))
}

/// Faithful decode of a Gemini `generateContent` body (TD-0010 D4b).
///
/// This is the mirror of `sandhi-providers::gemini_typed`'s request encoder — which is what makes
/// a round trip meaningful: whatever that encoder produces from a `ChatRequestV1`, this reads back
/// into the same request. D4a shipped an accounting-grade decode and *refused* cross-family rather
/// than translate from it, because dropping tools or media silently changes the caller's request.
/// This lifts that refusal by preserving what carries meaning, and by keeping everything it does
/// not model verbatim in `extensions` rather than on the floor.
fn decode_gemini_request(
    body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    let object = body
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;

    let mut messages = Vec::new();
    if let Some(text) = object
        .get("systemInstruction")
        .and_then(|s| gemini_parts_text(s.get("parts")))
    {
        messages.push(ChatMessageV1::System {
            content: MessageContent::Text(text),
            name: None,
        });
    }

    for content in object
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(|| "contents must be an array".to_string())?
    {
        let parts: &[Value] = content
            .get("parts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        // Split at tool results, preserving text/media on both sides in source order.
        let mut start = 0;
        for (index, part) in parts.iter().enumerate() {
            let Some(fr) = part.get("functionResponse") else {
                continue;
            };
            if start < index {
                messages.push(decode_gemini_segment(content, &parts[start..index])?);
            }
            let tool_call_id = fr
                .get("id")
                .or_else(|| fr.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let response = fr.get("response").unwrap_or(&Value::Null);
            // Invert our own one-field wrapper, but retain arbitrary native objects
            // and additional fields instead of replacing them with null.
            let output = if response
                .as_object()
                .is_some_and(|object| object.len() == 1 && object.contains_key("output"))
            {
                &response["output"]
            } else {
                response
            };
            messages.push(ChatMessageV1::Tool {
                content: MessageContent::Text(match output {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                }),
                tool_call_id,
            });
            start = index + 1;
        }
        if start < parts.len() || parts.is_empty() {
            messages.push(decode_gemini_segment(content, &parts[start..])?);
        }
    }

    let tools: Vec<ToolDefinitionV1> = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("functionDeclarations"))
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(|d| serde_json::from_value::<ToolDefinitionV1>(d.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    let tool_choice = object
        .get("toolConfig")
        .and_then(|tc| tc.get("functionCallingConfig"))
        .and_then(|config| {
            let mode = config.get("mode").and_then(Value::as_str)?;
            Some(match mode {
                "NONE" => ToolChoiceV1::Mode(ToolChoiceMode::None),
                // Gemini's ANY means "call something"; with an allow-list of one it is the
                // neutral contract's named-function choice.
                "ANY" => match config
                    .get("allowedFunctionNames")
                    .and_then(Value::as_array)
                    .and_then(|names| names.first())
                    .and_then(Value::as_str)
                {
                    Some(name) => ToolChoiceV1::Function {
                        name: name.to_string(),
                    },
                    None => ToolChoiceV1::Mode(ToolChoiceMode::Required),
                },
                _ => ToolChoiceV1::Mode(ToolChoiceMode::Auto),
            })
        });

    let generation = object.get("generationConfig").and_then(Value::as_object);
    let temperature = generation
        .and_then(|g| g.get("temperature"))
        .and_then(Value::as_f64);
    let max_output_tokens = generation
        .and_then(|g| g.get("maxOutputTokens"))
        .and_then(Value::as_u64);
    let stop = generation
        .and_then(|g| g.get("stopSequences"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        });

    // Anything this codec does not model — safetySettings, cachedContent, responseSchema,
    // topP/topK — is kept verbatim so it survives translation. It is stashed under
    // `extensions["gemini"]` (W3d/G7 D5) — the SAME key `encode_gemini_request` clones as its
    // base body — so a gemini->typed->gemini round-trip is lossless. (The pre-D5 flat stash put
    // these at `extensions["topP"]` / `extensions["generationConfig"]`, which the encoder never
    // read, silently dropping them on re-encode.)
    let mut gemini_body = Map::new();
    for (key, value) in object {
        if !matches!(
            key.as_str(),
            "contents"
                | "systemInstruction"
                | "tools"
                | "toolConfig"
                | "generationConfig"
                | "model"
                // Gateway-reserved and interpreted only on the OpenAI dialect
                // (ADR-0013) — never let it ride the gemini body clone (the
                // translation plane's re-encode base) to an upstream.
                | "sandhi_extensions"
        ) {
            gemini_body.insert(key.clone(), value.clone());
        }
    }
    if let Some(generation) = generation {
        let leftovers: Map<String, Value> = generation
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "temperature" | "maxOutputTokens" | "stopSequences"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if !leftovers.is_empty() {
            gemini_body.insert("generationConfig".to_string(), Value::Object(leftovers));
        }
    }
    let mut extensions = BTreeMap::new();

    // Retain exact input sequences alongside the existing unmodelled extras.
    // Generation controls remain projected/merged as before.
    for key in ["contents", "systemInstruction", "tools", "toolConfig"] {
        if let Some(value) = object.get(key) {
            gemini_body.insert(key.into(), value.clone());
        }
    }
    extensions.insert("gemini".into(), Value::Object(gemini_body));
    let mut request = ChatRequestV1 {
        schema_version: CHAT_SCHEMA_VERSION_V1.to_string(),
        // Stamped by the handler from the path segment.
        model: String::new(),
        messages,
        tools,
        tool_choice,
        temperature,
        max_output_tokens,
        stop,
        response_format: None,
        seed: None,
        metadata,
        // W3d/G7: Gemini has no effort concept; thinking rides
        // generationConfig.thinkingConfig.thinkingBudget.
        reasoning_effort: None,
        thinking: lift_gemini_thinking(object)?,
        // #90's native-response gate, matching every other dialect's decode.
        include_native_response: true,
        include_native_stream: false,
        native_input: None,
        cache_intent: None,
        compiled_cache: None,
        // D4b: the populated map — unmodelled Gemini keys survive here rather than being dropped.
        extensions,
    };
    fidelity::capture(
        &mut request,
        sandhi_core::EndpointFamilyV1::GeminiGenerateContent,
    )?;
    // Streaming is a path verb, not a body field; the handler stamps it.
    Ok((request, false))
}

fn decode_gemini_segment(content: &Value, parts: &[Value]) -> Result<ChatMessageV1, String> {
    let decoded = gemini_content(parts)?;
    Ok(
        if content.get("role").and_then(Value::as_str) == Some("model") {
            ChatMessageV1::Assistant {
                content: decoded,
                name: None,
                tool_calls: parts
                    .iter()
                    .filter_map(|part| part.get("functionCall"))
                    .map(|call| {
                        let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                        ToolCallV1 {
                            id: call
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or(name)
                                .into(),
                            name: name.into(),
                            arguments: call
                                .get("args")
                                .map(ToString::to_string)
                                .unwrap_or_else(|| "{}".into()),
                            extensions: BTreeMap::new(),
                        }
                    })
                    .collect(),
                refusal: None,
            }
        } else {
            ChatMessageV1::User {
                content: decoded.unwrap_or_else(|| MessageContent::Text(String::new())),
                name: None,
            }
        },
    )
}

/// ProtoJSON accepts the JSON and proto field names. Both consumers use one
/// decision so a valid SDK image cannot be classified as unsupported media.
fn gemini_inline_mime(inline: &Value) -> Result<&str, String> {
    let camel = inline.get("mimeType");
    let snake = inline.get("mime_type");
    if camel.is_some() && snake.is_some() && camel != snake {
        return Err("Gemini inlineData contains conflicting MIME aliases".into());
    }
    camel
        .or(snake)
        .and_then(Value::as_str)
        .filter(|mime| {
            let token = |value: &str| {
                !value.is_empty()
                    && value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
                    })
            };
            mime.split_once('/')
                .is_some_and(|(kind, subtype)| token(kind) && token(subtype))
        })
        .ok_or_else(|| "Gemini inlineData requires a valid MIME type".into())
}

/// Gemini `parts[]` → neutral content. Text-only collapses to `Text`; anything richer keeps its
/// parts, with `inlineData` rendered as the data URI the neutral contract carries images in.
fn gemini_content(parts: &[Value]) -> Result<Option<MessageContent>, String> {
    let mut collected: Vec<ContentPart> = Vec::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            collected.push(ContentPart::Text {
                text: text.to_string(),
            });
        } else if let Some(inline) = part.get("inlineData") {
            let mime = gemini_inline_mime(inline)?.to_ascii_lowercase();
            let data = inline
                .get("data")
                .and_then(Value::as_str)
                .filter(|data| !data.is_empty())
                .ok_or_else(|| "Gemini inlineData requires base64 bytes".to_string())?;
            use base64::engine::general_purpose::{
                STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
            };
            let urlsafe = data.bytes().any(|byte| matches!(byte, b'-' | b'_'));
            let engine = match (urlsafe, data.len() % 4 == 0) {
                (false, true) => &STANDARD,
                (false, false) => &STANDARD_NO_PAD,
                (true, true) => &URL_SAFE,
                (true, false) => &URL_SAFE_NO_PAD,
            };
            let bytes = engine
                .decode(data)
                .map_err(|_| "Gemini inlineData has invalid base64 bytes".to_string())?;
            let data = STANDARD.encode(bytes);
            collected.push(ContentPart::ImageUrl {
                image_url: format!("data:{mime};base64,{data}"),
                detail: None,
            });
        }
    }
    Ok(match collected.len() {
        0 => None,
        _ if collected
            .iter()
            .all(|part| matches!(part, ContentPart::Text { .. })) =>
        {
            let text = collected
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            Some(MessageContent::Text(text))
        }
        _ => Some(MessageContent::Parts(collected)),
    })
}

/// Concatenate the `text` parts of a Gemini `parts[]` array.
fn gemini_parts_text(parts: Option<&Value>) -> Option<String> {
    let parts = parts?.as_array()?;
    let text: String = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

/// `ChatResponseV1` → a Gemini `generateContent` response (TD-0010 D4b).
///
/// Needed only on the CROSS-FAMILY path: a Gemini client talking to a Gemini upstream gets the
/// upstream's own bytes back verbatim (D4a). When the upstream is another family, the client
/// still expects Gemini's shape, so this is the mirror of the adapter's response parser.
fn object_tool_arguments(arguments: &str) -> Result<Value, &'static str> {
    let value = sandhi_core::policy::strict_json(arguments.as_bytes())
        .map_err(|_| "invalid or incomplete tool arguments cannot be translated to an object")?;
    if !value.is_object() {
        return Err("tool arguments must be an explicit JSON object");
    }
    Ok(value)
}

fn encode_gemini_response(response: &ChatResponseV1) -> Result<Value, &'static str> {
    let mut parts: Vec<Value> = Vec::new();
    if let Some(reasoning) = buffered_reasoning(response) {
        parts.push(json!({"text":reasoning,"thought":true}));
    }
    if let Some(content) = &response.output.content {
        match content {
            MessageContent::Text(text) if !text.is_empty() => parts.push(json!({ "text": text })),
            MessageContent::Text(_) => {}
            MessageContent::Parts(items) => {
                for item in items {
                    if let ContentPart::Text { text } = item {
                        parts.push(json!({ "text": text }));
                    }
                }
            }
        }
    }
    for call in &response.output.tool_calls {
        parts.push(json!({"functionCall": {
            "name": call.name,
            "id": call.id,
            // Gemini carries arguments as an OBJECT, not the JSON string the OpenAI family uses.
            "args": object_tool_arguments(&call.arguments)?,
        }}));
    }

    let mut candidate = json!({
        "content": {"role": "model", "parts": parts},
        "index": 0,
    });
    if let Some(reason) = response.finish_reason {
        candidate["finishReason"] = json!(gemini_finish_reason(reason));
    }
    Ok(json!({
        "candidates": [candidate],
        "usageMetadata": gemini_usage(&response.usage),
        "modelVersion": response.model,
    }))
}

/// Neutral finish reason → Gemini's enum name.
fn gemini_finish_reason(reason: FinishReasonV1) -> &'static str {
    match reason {
        FinishReasonV1::Stop => "STOP",
        FinishReasonV1::Length => "MAX_TOKENS",
        FinishReasonV1::ContentFilter => "SAFETY",
        // Gemini reports a tool turn as an ordinary stop; the functionCall part is the signal.
        FinishReasonV1::ToolCalls | FinishReasonV1::FunctionCall => "STOP",
        FinishReasonV1::Unknown => "FINISH_REASON_UNSPECIFIED",
    }
}

/// Neutral usage → Gemini's `usageMetadata`, preserving the cache split it reports natively.
fn gemini_usage(usage: &UsageV2) -> Value {
    let output = output_including_reasoning(usage);
    let reasoning = usage.reasoning_tokens.unwrap_or(0);
    let prompt = usage
        .tokens_in
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_creation_tokens);
    let mut meta = json!({
        "promptTokenCount": prompt,
        "candidatesTokenCount": output.saturating_sub(reasoning),
        "totalTokenCount": prompt.saturating_add(output),
    });
    if usage.cache_read_tokens > 0 {
        meta["cachedContentTokenCount"] = json!(usage.cache_read_tokens);
    }
    if let Some(reasoning) = usage.reasoning_tokens {
        meta["thoughtsTokenCount"] = json!(reasoning);
    }
    meta
}

/// Canonical stream event → Gemini streaming chunks (`?alt=sse` framing).
///
/// Gemini has no per-event envelope: every chunk is a whole `GenerateContentResponse` carrying a
/// delta, so a text delta and a finish become separate chunks of the same shape. Events with no
/// Gemini representation (tool-call scaffolding) emit nothing rather
/// than inventing a frame the client would mis-parse.
fn encode_gemini_stream_event(
    event: &ChatStreamEventV1,
    last_usage: Option<&UsageV2>,
) -> Vec<(Option<&'static str>, Value)> {
    match event {
        ChatStreamEventV1::TextDelta { delta } => vec![(
            None,
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":delta}]},"index":0}]}),
        )],
        ChatStreamEventV1::ReasoningDelta { delta } => vec![(
            None,
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":delta,"thought":true}]},"index":0}]}),
        )],
        // Gemini SSE surfaces failures as an `{"error": ...}` data frame, not a
        // dropped stream — previously this fell into the silent catch-all.
        ChatStreamEventV1::Error { error } => {
            let status = error
                .http_status
                .and_then(|s| StatusCode::from_u16(s).ok())
                .filter(|s| s.is_client_error() || s.is_server_error())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            vec![(
                None,
                json!({"error":{"code":status.as_u16(),"message":error.message,"status":google_status(status)}}),
            )]
        }
        ChatStreamEventV1::Finish { reason } => {
            let mut chunk = json!({"candidates":[{
                "content": {"role": "model", "parts": []},
                "finishReason": gemini_finish_reason(*reason),
                "index": 0,
            }]});
            // The terminal chunk carries usage — where a Gemini client reads it from.
            if let Some(usage) = last_usage {
                chunk["usageMetadata"] = gemini_usage(usage);
            }
            vec![(None, chunk)]
        }
        // Tool-call events never reach this stateless encoder: the stateful
        // `StreamEventEncoder::encode_gemini` buffers them across start /
        // argument-deltas and emits ONE complete `functionCall` part at
        // `ToolCallEnd` (Gemini has no partial frame). ResponseStart, Usage
        // and reasoning deltas have no Gemini SSE frame — nothing to emit.
        _ => Vec::new(),
    }
}

/// Extract and validate the reserved `sandhi_extensions` passthrough field.
///
/// Returns the admitted family buckets and removes the field from `body` so it
/// never rides inside the decoded openai body clone. Guard rails (ADR-0013):
/// the map must be family-slug → JSON object, and the decode-owned `openai`
/// bucket is rejected so the ingress body clone stays authoritative. Unknown
/// family slugs are inert by construction — no encoder reads a bucket it does
/// not own.
fn decode_sandhi_extensions(body: &mut Value) -> Result<Vec<(String, Value)>, String> {
    let Some(value) = body.get("sandhi_extensions").cloned() else {
        return Ok(Vec::new());
    };
    let map = match value {
        Value::Object(map) => map,
        _ => {
            return Err(
                "sandhi_extensions must be a JSON object mapping family slugs to native request objects"
                    .to_string(),
            )
        }
    };
    let mut entries = Vec::with_capacity(map.len());
    for (key, bucket) in map {
        if key == "openai" {
            return Err(
                "sandhi_extensions cannot include the 'openai' bucket; it is owned by the decoded request body"
                    .to_string(),
            );
        }
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(format!(
                "sandhi_extensions key {key:?} is not a family slug (lowercase letters, digits, '-', '_')"
            ));
        }
        if !bucket.is_object() {
            return Err(format!("sandhi_extensions[{key}] must be a JSON object"));
        }
        entries.push((key, bucket));
    }
    body.as_object_mut()
        .ok_or_else(|| "request body must be a JSON object".to_string())?
        .remove("sandhi_extensions");
    Ok(entries)
}

/// Strip the gateway-reserved `sandhi_extensions` key from forwarded request
/// bytes (ADR-0013): the key is gateway-consumed and must never reach an
/// upstream on any plane. Rewrites the bytes only when the key is actually
/// present at the top level, so ordinary traffic stays byte-exact — a mention
/// inside message content parses to a nested position and leaves the bytes
/// untouched, and non-JSON or non-object bodies pass through unchanged.
pub(crate) fn strip_reserved_passthrough(body: bytes::Bytes) -> bytes::Bytes {
    const RESERVED: &[&[u8]] = &[b"sandhi_extensions", b"sandhi_cache"];
    // Fast path: the literal bytes are absent AND no JSON unicode escape is
    // present, so no spelling of the reserved key can exist — stay byte-exact
    // without a parse. Any non-literal spelling of an ASCII key requires at
    // least one `\uXXXX` escape, so checking for `\u` is a necessary condition
    // (bodies carrying escaped non-ASCII text simply pay one parse).
    let mentions_reserved = RESERVED
        .iter()
        .any(|key| body.windows(key.len()).any(|window| window == *key));
    let mentions_escape = body.windows(2).any(|w| w == b"\\u");
    if !(mentions_reserved || mentions_escape) {
        return body;
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(object) = value.as_object_mut() else {
        return body;
    };
    let removed_extensions = object.remove("sandhi_extensions").is_some();
    let removed_cache = object.remove("sandhi_cache").is_some();
    if removed_extensions || removed_cache {
        return bytes::Bytes::from(serde_json::to_vec(&value).expect("re-serializing parsed JSON"));
    }
    body
}

pub(crate) fn decode_openai_request(
    mut body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    // Guarded native passthrough (ADR-0013): OpenAI-compat clients exercise
    // family-native request extras (Anthropic `cache_control`, provider
    // `top_k`) through the reserved `sandhi_extensions` body field. Validated
    // here and merged into the typed `extensions` map — every family encoder
    // inserts the typed fields (model, messages, tools, sampling params) AFTER
    // cloning its bucket, so a passthrough bucket can only add native extras,
    // never override the decoded, validated contract.
    let passthrough = decode_sandhi_extensions(&mut body)?;
    let object = body
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let model = required_string(object, "model")?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages must be an array".to_string())?
        .iter()
        .map(decode_openai_message)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    let function = value
                        .get("function")
                        .and_then(Value::as_object)
                        .ok_or_else(|| "tools entries must contain function objects".to_string())?;
                    Ok(ToolDefinitionV1 {
                        name: required_string(function, "name")?,
                        description: optional_string(function, "description")?,
                        parameters: function
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object"})),
                        strict: function.get("strict").and_then(Value::as_bool),
                    })
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?
        .unwrap_or_default();
    let mut request = ChatRequestV1 {
        schema_version: CHAT_SCHEMA_VERSION_V1.into(),
        model,
        messages,
        tools,
        tool_choice: object
            .get("tool_choice")
            .map(decode_openai_tool_choice)
            .transpose()?,
        temperature: object.get("temperature").and_then(Value::as_f64),
        max_output_tokens: object
            .get("max_completion_tokens")
            .or_else(|| object.get("max_tokens"))
            .and_then(Value::as_u64),
        stop: decode_stop(object.get("stop"))?,
        response_format: object.get("response_format").cloned(),
        seed: object.get("seed").and_then(Value::as_i64),
        metadata,
        // W3d/G7: lift promoted params to typed fields so they survive a
        // cross-family route (they otherwise live only in the openai body clone).
        reasoning_effort: optional_string(object, "reasoning_effort")?,
        thinking: lift_openai_thinking(object),
        include_native_response: true,
        include_native_stream: false,
        native_input: None,
        cache_intent: None,
        compiled_cache: None,
        extensions: {
            let mut extensions = BTreeMap::from([("openai".into(), body.clone())]);
            for (family, bucket) in passthrough {
                extensions.insert(family, bucket);
            }
            extensions
        },
    };
    fidelity::capture(
        &mut request,
        sandhi_core::EndpointFamilyV1::OpenaiChatCompletions,
    )?;
    request.validate()?;
    Ok((
        request,
        object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

fn decode_openai_message(value: &Value) -> Result<ChatMessageV1, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "message must be an object".to_string())?;
    let role = required_string(object, "role")?;
    let name = optional_string(object, "name")?;
    match role.as_str() {
        "developer" => Ok(ChatMessageV1::Developer {
            content: decode_openai_content(required(object, "content")?)?,
            name,
        }),
        "system" => Ok(ChatMessageV1::System {
            content: decode_openai_content(required(object, "content")?)?,
            name,
        }),
        "user" => Ok(ChatMessageV1::User {
            content: decode_openai_content(required(object, "content")?)?,
            name,
        }),
        "assistant" => Ok(ChatMessageV1::Assistant {
            content: object
                .get("content")
                .filter(|value| !value.is_null())
                .map(decode_openai_content)
                .transpose()?,
            name,
            tool_calls: object
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| calls.iter().map(decode_openai_tool_call).collect())
                .transpose()?
                .unwrap_or_default(),
            refusal: optional_string(object, "refusal")?,
        }),
        "tool" => Ok(ChatMessageV1::Tool {
            content: decode_openai_content(required(object, "content")?)?,
            tool_call_id: required_string(object, "tool_call_id")?,
        }),
        "function" => Ok(ChatMessageV1::Function {
            content: decode_openai_content(required(object, "content")?)?,
            name: name.ok_or_else(|| "function message requires name".to_string())?,
        }),
        other => Err(format!("unsupported OpenAI message role: {other}")),
    }
}

fn decode_openai_content(value: &Value) -> Result<MessageContent, String> {
    if let Some(text) = value.as_str() {
        return Ok(MessageContent::Text(text.into()));
    }
    let parts = value
        .as_array()
        .ok_or_else(|| "message content must be a string or array".to_string())?;
    Ok(MessageContent::Parts(
        parts
            .iter()
            .map(|part| {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" | "input_text" => Ok(ContentPart::Text {
                        text: part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    }),
                    "image_url" => {
                        let image = part.get("image_url").unwrap_or(&Value::Null);
                        let image_url = image
                            .as_str()
                            .or_else(|| image.get("url").and_then(Value::as_str))
                            .ok_or_else(|| "image_url part requires a URL".to_string())?;
                        Ok(ContentPart::ImageUrl {
                            image_url: image_url.into(),
                            detail: image
                                .get("detail")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                    }
                    "input_audio" => {
                        let audio = part.get("input_audio").unwrap_or(&Value::Null);
                        Ok(ContentPart::InputAudio {
                            data: audio
                                .get("data")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "input_audio requires data".to_string())?
                                .into(),
                            format: audio
                                .get("format")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "input_audio requires format".to_string())?
                                .into(),
                        })
                    }
                    "file" | "input_file" => {
                        let file = part.get("file").unwrap_or(part);
                        Ok(ContentPart::File {
                            file_id: file
                                .get("file_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            file_data: file
                                .get("file_data")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            filename: file
                                .get("filename")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                    }
                    other => Err(format!("unsupported OpenAI content part: {other}")),
                }
            })
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn decode_openai_tool_call(value: &Value) -> Result<ToolCallV1, String> {
    Ok(ToolCallV1 {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool call requires id".to_string())?
            .into(),
        name: value
            .pointer("/function/name")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool call requires function.name".to_string())?
            .into(),
        arguments: value
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        extensions: BTreeMap::new(),
    })
}

fn decode_openai_tool_choice(value: &Value) -> Result<ToolChoiceV1, String> {
    if let Some(mode) = value.as_str() {
        return Ok(ToolChoiceV1::Mode(match mode {
            "none" => ToolChoiceMode::None,
            "auto" => ToolChoiceMode::Auto,
            "required" => ToolChoiceMode::Required,
            other => return Err(format!("unsupported tool_choice mode: {other}")),
        }));
    }
    value
        .pointer("/function/name")
        .and_then(Value::as_str)
        .map(|name| ToolChoiceV1::Function { name: name.into() })
        .ok_or_else(|| "tool_choice must be a mode or named function".to_string())
}

fn decode_anthropic_request(
    mut body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    // `sandhi_extensions` is gateway-reserved and interpreted only on the
    // OpenAI dialect (ADR-0013); drop it so the family body clone — the
    // translation plane's re-encode base — never carries the key upstream.
    if let Some(object) = body.as_object_mut() {
        object.remove("sandhi_extensions");
    }
    let object = body
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let mut messages = Vec::new();
    if let Some(system) = object.get("system") {
        messages.push(ChatMessageV1::System {
            content: decode_anthropic_text_content(system)?,
            name: None,
        });
    }
    for message in object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages must be an array".to_string())?
    {
        decode_anthropic_message(message, &mut messages)?;
    }
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    Ok(ToolDefinitionV1 {
                        name: tool
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "Anthropic tool requires name".to_string())?
                            .into(),
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        parameters: tool
                            .get("input_schema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object"})),
                        strict: None,
                    })
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?
        .unwrap_or_default();
    let mut request = ChatRequestV1 {
        schema_version: CHAT_SCHEMA_VERSION_V1.into(),
        model: required_string(object, "model")?,
        messages,
        tools,
        tool_choice: object
            .get("tool_choice")
            .map(decode_anthropic_tool_choice)
            .transpose()?,
        temperature: object.get("temperature").and_then(Value::as_f64),
        max_output_tokens: object.get("max_tokens").and_then(Value::as_u64),
        stop: decode_stop(object.get("stop_sequences"))?,
        response_format: None,
        seed: None,
        metadata,
        // W3d/G7: Anthropic `thinking` uses the same {type, budget_tokens}
        // shape; no effort concept.
        reasoning_effort: None,
        thinking: lift_openai_thinking(object),
        include_native_response: true,
        include_native_stream: false,
        native_input: None,
        cache_intent: None,
        compiled_cache: None,
        extensions: BTreeMap::from([("anthropic".into(), body.clone())]),
    };
    fidelity::capture(
        &mut request,
        sandhi_core::EndpointFamilyV1::AnthropicMessages,
    )?;
    request.validate()?;
    Ok((
        request,
        object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

fn decode_anthropic_message(
    value: &Value,
    messages: &mut Vec<ChatMessageV1>,
) -> Result<(), String> {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| "Anthropic message requires role".to_string())?;
    let content = value
        .get("content")
        .ok_or_else(|| "Anthropic message requires content".to_string())?;
    let blocks = if let Some(text) = content.as_str() {
        vec![json!({"type":"text", "text":text})]
    } else {
        content
            .as_array()
            .cloned()
            .ok_or_else(|| "Anthropic content must be a string or array".to_string())?
    };
    match role {
        "assistant" => {
            let mut parts = Vec::new();
            let mut calls = Vec::new();
            for block in &blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(ContentPart::Text {
                        text: block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    }),
                    Some("tool_use") => calls.push(ToolCallV1 {
                        id: block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        arguments: serde_json::to_string(
                            block.get("input").unwrap_or(&Value::Null),
                        )
                        .map_err(|error| error.to_string())?,
                        extensions: BTreeMap::new(),
                    }),
                    Some("thinking") | Some("redacted_thinking") => {}
                    Some(other) => {
                        return Err(format!("unsupported Anthropic assistant block: {other}"))
                    }
                    None => return Err("Anthropic content block requires type".into()),
                }
            }
            messages.push(ChatMessageV1::Assistant {
                content: content_from_parts(parts),
                name: None,
                tool_calls: calls,
                refusal: None,
            });
        }
        "user" => {
            let mut parts = Vec::new();
            for block in &blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    if !parts.is_empty() {
                        messages.push(ChatMessageV1::User {
                            content: MessageContent::Parts(std::mem::take(&mut parts)),
                            name: None,
                        });
                    }
                    messages.push(ChatMessageV1::Tool {
                        content: decode_anthropic_text_content(
                            block
                                .get("content")
                                .unwrap_or(&Value::String(String::new())),
                        )?,
                        tool_call_id: block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "tool_result requires tool_use_id".to_string())?
                            .into(),
                    });
                } else {
                    parts.push(decode_anthropic_part(block)?);
                }
            }
            if !parts.is_empty() {
                messages.push(ChatMessageV1::User {
                    content: MessageContent::Parts(parts),
                    name: None,
                });
            }
        }
        other => return Err(format!("unsupported Anthropic message role: {other}")),
    }
    Ok(())
}

fn decode_anthropic_text_content(value: &Value) -> Result<MessageContent, String> {
    if let Some(text) = value.as_str() {
        return Ok(MessageContent::Text(text.into()));
    }
    let blocks = value
        .as_array()
        .ok_or_else(|| "Anthropic text content must be a string or array".to_string())?;
    let mut text = String::new();
    for block in blocks {
        let piece = block
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| block.as_str())
            .ok_or_else(|| "expected Anthropic text block".to_string())?;
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(piece);
    }
    Ok(MessageContent::Text(text))
}

fn decode_anthropic_part(block: &Value) -> Result<ContentPart, String> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Ok(ContentPart::Text {
            text: block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
        }),
        Some("image") => {
            let source = block.get("source").unwrap_or(&Value::Null);
            let image_url = match source.get("type").and_then(Value::as_str) {
                Some("url") => source
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                Some("base64") => format!(
                    "data:{};base64,{}",
                    source
                        .get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream"),
                    source.get("data").and_then(Value::as_str).unwrap_or("")
                ),
                _ => return Err("unsupported Anthropic image source".into()),
            };
            Ok(ContentPart::ImageUrl {
                image_url,
                detail: None,
            })
        }
        Some(other) => Err(format!("unsupported Anthropic user block: {other}")),
        None => Err("Anthropic content block requires type".into()),
    }
}

fn decode_anthropic_tool_choice(value: &Value) -> Result<ToolChoiceV1, String> {
    match value.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(ToolChoiceV1::Mode(ToolChoiceMode::Auto)),
        Some("any") => Ok(ToolChoiceV1::Mode(ToolChoiceMode::Required)),
        Some("none") => Ok(ToolChoiceV1::Mode(ToolChoiceMode::None)),
        Some("tool") => Ok(ToolChoiceV1::Function {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic tool choice requires name".to_string())?
                .into(),
        }),
        Some(other) => Err(format!("unsupported Anthropic tool choice: {other}")),
        None => Err("Anthropic tool_choice requires type".into()),
    }
}

/// Decode an OpenAI Responses-API ingress body into the canonical `ChatRequestV1`.
///
/// This is the inverse of `openai_responses_typed::encode_responses_request`: the caller's
/// `instructions` + typed `input` items (`message` / `function_call` / `function_call_output`)
/// become the canonical message list, and `tools`/`tool_choice`/sampling fields map across. The
/// raw body is retained under `extensions["openai_responses"]` so a Responses-backend upstream can
/// re-read Responses-only fields (e.g. `reasoning.effort`) that have no canonical home.
fn decode_responses_request(
    mut body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    // Gateway-reserved, uninterpreted on this dialect (ADR-0013) — never let
    // it ride the openai_responses body clone to an upstream.
    if let Some(object) = body.as_object_mut() {
        object.remove("sandhi_extensions");
    }
    let object = body
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let mut messages = Vec::new();
    if let Some(instructions) = optional_string(object, "instructions")? {
        messages.push(ChatMessageV1::System {
            content: MessageContent::Text(instructions),
            name: None,
        });
    }
    match object.get("input") {
        Some(Value::String(text)) => messages.push(ChatMessageV1::User {
            content: MessageContent::Text(text.clone()),
            name: None,
        }),
        Some(Value::Array(items)) => {
            for item in items {
                decode_responses_input_item(item, &mut messages)?;
            }
        }
        Some(_) => return Err("Responses input must be a string or array".into()),
        None => {}
    }
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter(|value| {
                    value
                        .get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|kind| kind == "function")
                })
                .map(|value| {
                    let name = value
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| value.pointer("/function/name").and_then(Value::as_str))
                        .ok_or_else(|| "Responses tool requires name".to_string())?;
                    Ok(ToolDefinitionV1 {
                        name: name.into(),
                        description: value
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        parameters: value
                            .get("parameters")
                            .or_else(|| value.get("function").and_then(|f| f.get("parameters")))
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object"})),
                        strict: value.get("strict").and_then(Value::as_bool),
                    })
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?
        .unwrap_or_default();
    let mut request = ChatRequestV1 {
        schema_version: CHAT_SCHEMA_VERSION_V1.into(),
        model: required_string(object, "model")?,
        messages,
        tools,
        tool_choice: object
            .get("tool_choice")
            .map(decode_responses_tool_choice)
            .transpose()?,
        temperature: object.get("temperature").and_then(Value::as_f64),
        max_output_tokens: object.get("max_output_tokens").and_then(Value::as_u64),
        stop: None,
        response_format: object
            .get("text")
            .and_then(|text| text.get("format"))
            .map(responses_response_format)
            .transpose()?,
        seed: None,
        metadata,
        // W3d/G7: Responses expresses effort as `reasoning.effort`; no thinking.
        reasoning_effort: object
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        thinking: None,
        include_native_response: true,
        include_native_stream: false,
        native_input: None,
        cache_intent: None,
        compiled_cache: None,
        extensions: BTreeMap::from([("openai_responses".into(), body.clone())]),
    };
    fidelity::capture(&mut request, sandhi_core::EndpointFamilyV1::OpenaiResponses)?;
    request.validate()?;
    Ok((
        request,
        object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

fn decode_responses_input_item(
    item: &Value,
    messages: &mut Vec<ChatMessageV1>,
) -> Result<(), String> {
    // EasyInputMessage omits `type`; a role-bearing item is still a message.
    // Keep explicitly typed native items on their existing dispatch path.
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_else(|| {
        if item.get("role").is_some() {
            "message"
        } else {
            ""
        }
    });
    match kind {
        "message" => messages.push(decode_responses_message_item(item)?),
        "function_call" => {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .or_else(|| item.get("id").and_then(Value::as_str))
                .ok_or_else(|| "Responses function_call requires call_id".to_string())?;
            let name = required_string(
                item.as_object()
                    .ok_or_else(|| "Responses item must be an object".to_string())?,
                "name",
            )?;
            messages.push(ChatMessageV1::Assistant {
                content: None,
                name: None,
                tool_calls: vec![ToolCallV1 {
                    id: id.into(),
                    name,
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    extensions: BTreeMap::new(),
                }],
                refusal: None,
            });
        }
        "function_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Responses function_call_output requires call_id".to_string())?;
            let output = item.get("output").cloned().unwrap_or(Value::Null);
            let content = match &output {
                Value::String(text) => MessageContent::Text(text.clone()),
                other => MessageContent::Text(other.to_string()),
            };
            messages.push(ChatMessageV1::Tool {
                content,
                tool_call_id: call_id.into(),
            });
        }
        // The portable projection omits opaque/unknown items. Codec-owned native
        // input preserves their exact order; the fidelity gate refuses incompatible targets.
        _ => {}
    }
    Ok(())
}

fn decode_responses_message_item(item: &Value) -> Result<ChatMessageV1, String> {
    let object = item
        .as_object()
        .ok_or_else(|| "Responses message item must be an object".to_string())?;
    let role = required_string(object, "role")?;
    let raw_content = object.get("content").unwrap_or(&Value::Null);
    let parts = if let Some(text) = raw_content.as_str() {
        vec![json!({"type":"input_text","text":text})]
    } else {
        raw_content
            .as_array()
            .cloned()
            .ok_or_else(|| "Responses message content must be a string or array".to_string())?
    };
    let mut content_parts = Vec::new();
    let mut refusal = String::new();
    for part in &parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("text") => {
                content_parts.push(ContentPart::Text {
                    text: part
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                });
            }
            Some("refusal") => {
                refusal.push_str(part.get("refusal").and_then(Value::as_str).unwrap_or(""))
            }
            Some("input_image") => {
                if let Some(url) = part.get("image_url").and_then(Value::as_str) {
                    content_parts.push(ContentPart::ImageUrl {
                        image_url: url.into(),
                        detail: part
                            .get("detail")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    });
                }
                // Provider file IDs are retained in native input and marked
                // resource-bound by the fidelity gate, never treated as URLs.
            }
            Some("input_file") => content_parts.push(ContentPart::File {
                file_id: part
                    .get("file_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                file_data: part
                    .get("file_data")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                filename: part
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            _ => {}
        }
    }
    let content = content_from_parts(content_parts);
    match role.as_str() {
        "developer" => Ok(ChatMessageV1::Developer {
            content: content.unwrap_or_else(|| MessageContent::Text(String::new())),
            name: None,
        }),
        "system" => Ok(ChatMessageV1::System {
            content: content.unwrap_or_else(|| MessageContent::Text(String::new())),
            name: None,
        }),
        "user" => Ok(ChatMessageV1::User {
            content: content.unwrap_or_else(|| MessageContent::Text(String::new())),
            name: None,
        }),
        "assistant" => Ok(ChatMessageV1::Assistant {
            content,
            name: None,
            tool_calls: Vec::new(),
            refusal: (!refusal.is_empty()).then_some(refusal),
        }),
        other => Err(format!("unsupported Responses message role: {other}")),
    }
}

fn decode_responses_tool_choice(value: &Value) -> Result<ToolChoiceV1, String> {
    if let Some(mode) = value.as_str() {
        return Ok(ToolChoiceV1::Mode(match mode {
            "none" => ToolChoiceMode::None,
            "auto" => ToolChoiceMode::Auto,
            "required" => ToolChoiceMode::Required,
            other => return Err(format!("unsupported Responses tool_choice mode: {other}")),
        }));
    }
    value
        .get("name")
        .and_then(Value::as_str)
        .map(|name| ToolChoiceV1::Function { name: name.into() })
        .ok_or_else(|| "Responses tool_choice must be a mode or {type:function,name}".into())
}

/// Inverse of `responses_text_format`: recover the Chat Completions `response_format` from the
/// Responses `text.format` object.
fn responses_response_format(format: &Value) -> Result<Value, String> {
    if format.get("type").and_then(Value::as_str) == Some("json_schema") {
        Ok(json!({
            "type":"json_schema",
            "json_schema":{
                "name":format.get("name").cloned().unwrap_or(Value::Null),
                "schema":format.get("schema").cloned().unwrap_or(Value::Null),
                "strict":format.get("strict").cloned().unwrap_or(Value::Bool(false))
            }
        }))
    } else {
        Ok(format.clone())
    }
}

#[cfg(test)]
pub(crate) fn encode_response(
    dialect: IngressDialect,
    response: &ChatResponseV1,
) -> Result<Value, &'static str> {
    encode_response_with_emission(dialect, response).map(|(body, _)| body)
}

pub(crate) fn encode_response_with_emission(
    dialect: IngressDialect,
    response: &ChatResponseV1,
) -> Result<(Value, bool), &'static str> {
    // These bodies are decoder-owned provider responses, not request extras.
    // Compatible buffered egress must preserve signed/opaque state and unknown
    // native output parts when admission required typed request execution.
    let bucket = match dialect {
        IngressDialect::OpenAi => Some(("openai", "choices")),
        IngressDialect::Anthropic => Some(("anthropic", "content")),
        IngressDialect::Responses => Some(("openai_responses", "output")),
        IngressDialect::Gemini => Some(("gemini", "candidates")),
        _ => None,
    };
    if let Some(native) = bucket.and_then(|(bucket, field)| {
        response
            .extensions
            .get(bucket)
            .filter(|value| value.get(field).is_some_and(Value::is_array))
    }) {
        return Ok((native.clone(), true));
    }
    if response.output.content.as_ref().is_some_and(|content|matches!(content,
        MessageContent::Parts(parts) if parts.iter().any(|part|!matches!(part,ContentPart::Text{..})))) {
        return Err("buffered output media requires compatible native delivery");
    }
    if dialect == IngressDialect::Anthropic
        && response.finish_reason == Some(FinishReasonV1::ContentFilter)
    {
        return Err("buffered filtering semantics require a compatible client protocol");
    }
    if matches!(dialect, IngressDialect::Anthropic | IngressDialect::Gemini)
        && response
            .output
            .refusal
            .as_ref()
            .is_some_and(|text| !text.is_empty())
    {
        return Err("buffered refusal semantics require a compatible client protocol");
    }
    let body = match dialect {
        IngressDialect::OpenAi | IngressDialect::Embeddings | IngressDialect::Completions => {
            Ok(encode_openai_response(response))
        }
        IngressDialect::Anthropic => encode_anthropic_response(response),
        IngressDialect::Responses => Ok(encode_responses_response(response)),
        IngressDialect::Gemini => encode_gemini_response(response),
    };
    body.map(|body| (body, false))
}

fn encode_openai_response(response: &ChatResponseV1) -> Value {
    let mut message = Map::from_iter([("role".into(), Value::String("assistant".into()))]);
    message.insert(
        "content".into(),
        response
            .output
            .content
            .as_ref()
            .map(content_text)
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    if !response.output.tool_calls.is_empty() {
        message.insert(
            "tool_calls".into(),
            Value::Array(
                response
                    .output
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({"id":call.id,"type":"function","function":{
                            "name":call.name,"arguments":call.arguments
                        }})
                    })
                    .collect(),
            ),
        );
    }
    if let Some(refusal) = &response.output.refusal {
        message.insert("refusal".into(), Value::String(refusal.clone()));
    }
    if let Some(reasoning) = buffered_reasoning(response) {
        message.insert("reasoning_content".into(), reasoning.into());
    }
    json!({
        "id": response.id,
        "object":"chat.completion",
        "created":unix_now(),
        "model":response.model,
        "choices":[{"index":0,"message":message,"finish_reason":openai_finish(response.finish_reason)}],
        "usage":openai_usage(&response.usage),
    })
}

fn encode_anthropic_response(response: &ChatResponseV1) -> Result<Value, &'static str> {
    let mut content = Vec::new();
    if let Some(reasoning) = buffered_reasoning(response) {
        // Presentation of materialized text only, never a provider signature.
        content.push(json!({"type":"thinking","thinking":reasoning,"signature":""}));
    }
    if let Some(text) = response.output.content.as_ref().map(content_text) {
        if !text.is_empty() {
            content.push(json!({"type":"text","text":text}));
        }
    }
    for call in &response.output.tool_calls {
        let input = object_tool_arguments(&call.arguments)?;
        content.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":input}));
    }
    Ok(json!({
        "id":response.id,
        "type":"message",
        "role":"assistant",
        "model":response.model,
        "content":content,
        "stop_reason":anthropic_finish(response.finish_reason),
        "stop_sequence":Value::Null,
        "usage":anthropic_usage(&response.usage),
    }))
}

fn buffered_reasoning(response: &ChatResponseV1) -> Option<&str> {
    response
        .extensions
        .get("reasoning")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Encode the canonical response as an OpenAI Responses document — the inverse of
/// `openai_responses_typed::decode_responses_response`. The neutral assistant output becomes
/// `output` items (a `message` with `output_text` content and/or `function_call` items), with
/// `status` derived from the finish reason and a Responses-shaped `usage` block.
fn encode_responses_response(response: &ChatResponseV1) -> Value {
    let mut output = Vec::new();
    if let Some(reasoning) = buffered_reasoning(response) {
        output.push(json!({"type":"reasoning","id":format!("reasoning_{}",response.id.as_deref().unwrap_or("buffered")),
            "summary":[{"type":"summary_text","text":reasoning}],"status":"completed"}));
    }
    let text = response.output.content.as_ref().map(content_text);
    let has_text = text.as_deref().is_some_and(|text| !text.is_empty());
    let has_refusal = response
        .output
        .refusal
        .as_deref()
        .is_some_and(|refusal| !refusal.is_empty());
    if has_text || has_refusal {
        let mut content = Vec::new();
        if let Some(text) = text {
            if !text.is_empty() {
                content.push(json!({"type":"output_text","text":text}));
            }
        }
        if let Some(refusal) = &response.output.refusal {
            if !refusal.is_empty() {
                content.push(json!({"type":"refusal","refusal":refusal}));
            }
        }
        output.push(json!({"type":"message","role":"assistant","content":content}));
    }
    for call in &response.output.tool_calls {
        output.push(json!({
            "type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments
        }));
    }
    let status = responses_status(response.finish_reason);
    let mut body = json!({
        "id":response.id,
        "object":"response",
        "model":response.model,
        "status":status,
        "output":output,
        "usage":responses_usage(&response.usage),
    });
    if matches!(response.finish_reason, Some(FinishReasonV1::Length)) {
        body["incomplete_details"] = json!({"reason":"max_output_tokens"});
    } else if matches!(response.finish_reason, Some(FinishReasonV1::ContentFilter)) {
        body["incomplete_details"] = json!({"reason":"content_filter"});
    }
    body
}

fn responses_status(reason: Option<FinishReasonV1>) -> &'static str {
    match reason {
        Some(FinishReasonV1::Length) | Some(FinishReasonV1::ContentFilter) => "incomplete",
        // Stop, ToolCalls, FunctionCall, Unknown, and None all surface as a completed run.
        _ => "completed",
    }
}

fn responses_usage(usage: &UsageV2) -> Value {
    let input = usage
        .tokens_in
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_creation_tokens);
    let output = output_including_reasoning(usage);
    json!({
        "input_tokens":input,
        "output_tokens":output,
        "total_tokens":input.saturating_add(output),
        "input_tokens_details":openai_input_details(usage),
        "output_tokens_details":{
            "reasoning_tokens":usage.reasoning_tokens.unwrap_or(0)
        }
    })
}

/// Per-stream egress state for the dialects whose SSE protocols are stateful.
///
/// Anthropic's stream is a content-BLOCK protocol: every block needs an explicit
/// `content_block_start` with a unique index before its deltas and a
/// `content_block_stop` after them, and thinking vs text are DIFFERENT block
/// types. A stateless per-event encoder cannot express that — which SDKs
/// tolerate loosely but the spec does not. Gemini has the opposite constraint:
/// a function call is ONE complete part per chunk (no partial frame exists), so
/// tool calls must be buffered from start/argument-deltas and emitted complete
/// at `ToolCallEnd`. Responses retains bounded, ordered output for its terminal
/// fold. Chat Completions delegates to its stateless chunk encoder.
pub(crate) struct StreamEventEncoder {
    dialect: IngressDialect,
    /// Next Anthropic content-block index (one space shared by prose + tools).
    next_block: usize,
    /// The open prose (non-tool) block: (block index, "text" | "thinking").
    open_prose: Option<(usize, &'static str)>,
    /// Open tool blocks: canonical tool-call index → assigned block index.
    open_tools: BTreeMap<u32, usize>,
    /// Gemini buffered calls: canonical tool-call index → (id, name, args JSON).
    gemini_calls: BTreeMap<u32, (String, String, String)>,
    responses: responses_stream::Encoder,
    /// OpenAI chunk identity (id/model from ResponseStart), stamped on every
    /// subsequent chunk — the spec requires them on all frames, and a client
    /// strict enough to require `created` requires these too.
    openai_id: Option<String>,
    openai_model: Option<String>,
}

impl StreamEventEncoder {
    pub(crate) fn new(dialect: IngressDialect) -> Self {
        Self {
            dialect,
            next_block: 0,
            open_prose: None,
            open_tools: BTreeMap::new(),
            gemini_calls: BTreeMap::new(),
            responses: responses_stream::Encoder::new(),
            openai_id: None,
            openai_model: None,
        }
    }

    pub(crate) fn encode(
        &mut self,
        event: &ChatStreamEventV1,
        last_usage: Option<&UsageV2>,
    ) -> Result<Vec<(Option<&'static str>, Value)>, &'static str> {
        if matches!(
            self.dialect,
            IngressDialect::Anthropic | IngressDialect::Gemini
        ) && matches!(event, ChatStreamEventV1::RefusalDelta{delta} if !delta.is_empty())
        {
            return Err("streamed refusal semantics require a compatible client protocol");
        }
        if self.dialect == IngressDialect::Anthropic
            && matches!(
                event,
                ChatStreamEventV1::Finish {
                    reason: FinishReasonV1::ContentFilter
                }
            )
        {
            return Err("streamed filtering semantics require a compatible client protocol");
        }
        match self.dialect {
            IngressDialect::Anthropic => Ok(self.encode_anthropic(event, last_usage)),
            IngressDialect::Gemini => self.encode_gemini(event, last_usage),
            IngressDialect::Responses => self.responses.encode(event, last_usage),
            _ => Ok(self.encode_openai(event, last_usage)),
        }
    }

    /// Anthropic: maintain the block lifecycle. SDK stream accumulators read
    /// usage on every message_delta, including the stop delta.
    fn encode_anthropic(
        &mut self,
        event: &ChatStreamEventV1,
        last_usage: Option<&UsageV2>,
    ) -> Vec<(Option<&'static str>, Value)> {
        let mut frames: Vec<(Option<&'static str>, Value)> = Vec::new();
        match event {
            ChatStreamEventV1::NativeFrame { .. } => {}
            ChatStreamEventV1::ResponseStart { id, model } => {
                frames.push((
                    Some("message_start"),
                    json!({"type":"message_start","message":{
                        "id":id,"type":"message","role":"assistant","model":model,"content":[],
                        "stop_reason":Value::Null,"stop_sequence":Value::Null,
                        "usage":{"input_tokens":0,"output_tokens":0}
                    }}),
                ));
                // Spec-shaped keepalive; SDKs ignore it, strict consumers expect it.
                frames.push((Some("ping"), json!({"type":"ping"})));
            }
            ChatStreamEventV1::TextDelta { delta } => {
                let index = self.open_prose_block("text", &mut frames);
                frames.push((
                    Some("content_block_delta"),
                    json!({"type":"content_block_delta","index":index,
                        "delta":{"type":"text_delta","text":delta}}),
                ));
            }
            ChatStreamEventV1::ReasoningDelta { delta } => {
                let index = self.open_prose_block("thinking", &mut frames);
                frames.push((
                    Some("content_block_delta"),
                    json!({"type":"content_block_delta","index":index,
                        "delta":{"type":"thinking_delta","thinking":delta}}),
                ));
            }
            ChatStreamEventV1::RefusalDelta { delta } => {
                let index = self.open_prose_block("text", &mut frames);
                frames.push((
                    Some("content_block_delta"),
                    json!({"type":"content_block_delta","index":index,
                        "delta":{"type":"text_delta","text":delta}}),
                ));
            }
            ChatStreamEventV1::ToolCallStart { index, id, name } => {
                self.close_prose_block(&mut frames);
                let block = self.next_block;
                self.next_block += 1;
                self.open_tools.insert(*index, block);
                frames.push((
                    Some("content_block_start"),
                    json!({"type":"content_block_start","index":block,"content_block":{
                        "type":"tool_use","id":id,"name":name,"input":{}
                    }}),
                ));
            }
            ChatStreamEventV1::ToolCallArgumentsDelta { index, delta } => {
                let block = self
                    .open_tools
                    .get(index)
                    .copied()
                    .unwrap_or(*index as usize);
                frames.push((
                    Some("content_block_delta"),
                    json!({"type":"content_block_delta","index":block,"delta":{
                        "type":"input_json_delta","partial_json":delta
                    }}),
                ));
            }
            ChatStreamEventV1::ToolCallEnd { index } => {
                let block = self.open_tools.remove(index).unwrap_or(*index as usize);
                frames.push((
                    Some("content_block_stop"),
                    json!({"type":"content_block_stop","index":block}),
                ));
            }
            ChatStreamEventV1::Usage { usage } => {
                // Final usage arrives after source EOF. Messages deltas follow all
                // block stops; leaving text open here breaks strict gateway clients.
                self.close_prose_block(&mut frames);
                for (_, block) in std::mem::take(&mut self.open_tools) {
                    frames.push((
                        Some("content_block_stop"),
                        json!({"type":"content_block_stop","index":block}),
                    ));
                }
                frames.push((
                    Some("message_delta"),
                    json!({"type":"message_delta","delta":{},"usage":anthropic_usage(usage)}),
                ));
            }
            ChatStreamEventV1::Finish { reason } => {
                self.close_prose_block(&mut frames);
                let open: Vec<usize> = self.open_tools.values().copied().collect();
                self.open_tools.clear();
                for block in open {
                    frames.push((
                        Some("content_block_stop"),
                        json!({"type":"content_block_stop","index":block}),
                    ));
                }
                frames.push((
                    Some("message_delta"),
                    json!({"type":"message_delta","delta":{
                        "stop_reason":anthropic_finish(Some(*reason)),"stop_sequence":Value::Null
                    },"usage":last_usage.map(anthropic_usage).unwrap_or_else(||anthropic_usage(&UsageV2::default()))}),
                ));
                frames.push((Some("message_stop"), json!({"type":"message_stop"})));
            }
            ChatStreamEventV1::Error { error } => {
                // Close what is open so the SDK's block bookkeeping stays sane,
                // then surface the error event.
                self.close_prose_block(&mut frames);
                let open: Vec<usize> = self.open_tools.values().copied().collect();
                self.open_tools.clear();
                for block in open {
                    frames.push((
                        Some("content_block_stop"),
                        json!({"type":"content_block_stop","index":block}),
                    ));
                }
                frames.push((Some("error"), json!({"type":"error","error":error})));
            }
        }
        frames
    }

    /// Open (or reuse) the prose block of `kind`, emitting start/stop frames as
    /// the delta kinds switch between text and thinking.
    fn open_prose_block(
        &mut self,
        kind: &'static str,
        frames: &mut Vec<(Option<&'static str>, Value)>,
    ) -> usize {
        if let Some((index, open_kind)) = self.open_prose {
            if open_kind == kind {
                return index;
            }
            frames.push((
                Some("content_block_stop"),
                json!({"type":"content_block_stop","index":index}),
            ));
        }
        let index = self.next_block;
        self.next_block += 1;
        self.open_prose = Some((index, kind));
        let content_block = match kind {
            "thinking" => json!({"type":"thinking","thinking":""}),
            _ => json!({"type":"text","text":""}),
        };
        frames.push((
            Some("content_block_start"),
            json!({"type":"content_block_start","index":index,"content_block":content_block}),
        ));
        index
    }

    fn close_prose_block(&mut self, frames: &mut Vec<(Option<&'static str>, Value)>) {
        if let Some((index, _)) = self.open_prose.take() {
            frames.push((
                Some("content_block_stop"),
                json!({"type":"content_block_stop","index":index}),
            ));
        }
    }

    /// OpenAI: identical frames to the stateless encoder, plus the chunk
    /// identity (id/model captured at ResponseStart) on every subsequent
    /// chunk — the spec puts them on all frames, not only the first.
    fn encode_openai(
        &mut self,
        event: &ChatStreamEventV1,
        last_usage: Option<&UsageV2>,
    ) -> Vec<(Option<&'static str>, Value)> {
        if let ChatStreamEventV1::ResponseStart { id, model } = event {
            self.openai_id = id.clone();
            self.openai_model = Some(model.clone());
        }
        let mut frames = encode_openai_stream_event(event, last_usage);
        for (_, value) in frames.iter_mut() {
            if value.get("object").is_some() {
                if let Some(id) = &self.openai_id {
                    value["id"] = json!(id);
                }
                if let Some(model) = &self.openai_model {
                    value["model"] = json!(model);
                }
            }
        }
        frames
    }

    /// Gemini: buffer tool calls to whole `functionCall` parts (the protocol
    /// has no partial frame), and render an error frame (previously dropped).
    fn encode_gemini(
        &mut self,
        event: &ChatStreamEventV1,
        last_usage: Option<&UsageV2>,
    ) -> Result<Vec<(Option<&'static str>, Value)>, &'static str> {
        let frames = match event {
            ChatStreamEventV1::ToolCallStart { index, id, name } => {
                if self.gemini_calls.contains_key(index) {
                    return Err("duplicate tool-call start cannot be translated to Gemini");
                }
                self.gemini_calls
                    .insert(*index, (id.clone(), name.clone(), String::new()));
                Vec::new()
            }
            ChatStreamEventV1::ToolCallArgumentsDelta { index, delta } => {
                if let Some(call) = self.gemini_calls.get_mut(index) {
                    call.2.push_str(delta);
                } else {
                    return Err(
                        "tool arguments without a matching start cannot be translated to Gemini",
                    );
                }
                Vec::new()
            }
            ChatStreamEventV1::ToolCallEnd { index } => match self.gemini_calls.remove(index) {
                Some((id, name, args)) => {
                    let args_value = object_tool_arguments(&args)?;
                    vec![(
                        None,
                        json!({"candidates":[{
                            "content":{"role":"model","parts":[{"functionCall":{
                                "name":name,"id":id,"args":args_value
                            }}]},
                            "index":0
                        }]}),
                    )]
                }
                None => {
                    return Err(
                        "tool-call end without a matching start cannot be translated to Gemini",
                    )
                }
            },
            other => encode_gemini_stream_event(other, last_usage),
        };
        Ok(frames)
    }
}

fn encode_openai_stream_event(
    event: &ChatStreamEventV1,
    _last_usage: Option<&UsageV2>,
) -> Vec<(Option<&'static str>, Value)> {
    let value = match event {
        ChatStreamEventV1::NativeFrame { .. } => return Vec::new(),
        ChatStreamEventV1::ResponseStart { id, model } => json!({
            "id":id,"object":"chat.completion.chunk","created":unix_now(),"model":model,
            "choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":Value::Null}]
        }),
        ChatStreamEventV1::TextDelta { delta } => openai_delta(json!({"content":delta})),
        ChatStreamEventV1::ReasoningDelta { delta } => {
            openai_delta(json!({"reasoning_content":delta}))
        }
        ChatStreamEventV1::RefusalDelta { delta } => openai_delta(json!({"refusal":delta})),
        ChatStreamEventV1::ToolCallStart { index, id, name } => openai_delta(json!({
            "tool_calls":[{"index":index,"id":id,"type":"function","function":{"name":name,"arguments":""}}]
        })),
        ChatStreamEventV1::ToolCallArgumentsDelta { index, delta } => openai_delta(json!({
            "tool_calls":[{"index":index,"function":{"arguments":delta}}]
        })),
        ChatStreamEventV1::ToolCallEnd { .. } => return Vec::new(),
        ChatStreamEventV1::Usage { usage } => {
            json!({"object":"chat.completion.chunk","created":unix_now(),"choices":[],"usage":openai_usage(usage)})
        }
        ChatStreamEventV1::Finish { reason } => json!({
            "object":"chat.completion.chunk","created":unix_now(),
            "choices":[{"index":0,"delta":{},"finish_reason":openai_finish(Some(*reason))}]
        }),
        ChatStreamEventV1::Error { error } => json!({"error":error}),
    };
    vec![(None, value)]
}

/// Spec-required `created` unix timestamp for OpenAI completion objects and
/// chunks (the OpenAI SDKs declare the field required; omitting it breaks
/// strict client validation).
fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn openai_delta(delta: Value) -> Value {
    json!({"object":"chat.completion.chunk","created":unix_now(),"choices":[{"index":0,"delta":delta,"finish_reason":Value::Null}]})
}

fn output_including_reasoning(usage: &UsageV2) -> u64 {
    sandhi_core::billable_parts_with_reasoning(
        0,
        0,
        0,
        usage.tokens_out,
        usage.reasoning_tokens.unwrap_or(0),
        usage.reasoning_included,
    )
}

fn openai_input_details(usage: &UsageV2) -> Value {
    let mut details = json!({"cached_tokens":usage.cache_read_tokens});
    // Preserve legacy response shapes when no writes are measured. The neutral
    // contract has no write-field provenance, so zero cannot establish reporting.
    if usage.cache_creation_tokens > 0 {
        details["cache_write_tokens"] = json!(usage.cache_creation_tokens);
    }
    if let Some(audio) = usage.audio_input_tokens {
        details["audio_tokens"] = json!(audio);
    }
    details
}

fn openai_output_details(usage: &UsageV2) -> Value {
    let mut details = Map::new();
    for (name, value) in [
        ("reasoning_tokens", usage.reasoning_tokens),
        ("audio_tokens", usage.audio_output_tokens),
        (
            "accepted_prediction_tokens",
            usage.accepted_prediction_tokens,
        ),
        (
            "rejected_prediction_tokens",
            usage.rejected_prediction_tokens,
        ),
    ] {
        if let Some(value) = value {
            details.insert(name.into(), json!(value));
        }
    }
    Value::Object(details)
}

fn openai_usage(usage: &UsageV2) -> Value {
    let prompt_tokens = usage
        .tokens_in
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_creation_tokens);
    let output = output_including_reasoning(usage);
    json!({
        "prompt_tokens":prompt_tokens,
        "completion_tokens":output,
        "total_tokens":prompt_tokens.saturating_add(output),
        "prompt_tokens_details":openai_input_details(usage),
        "completion_tokens_details":openai_output_details(usage)
    })
}

fn anthropic_usage(usage: &UsageV2) -> Value {
    json!({
        "input_tokens":usage.tokens_in,
        "output_tokens":output_including_reasoning(usage),
        "cache_creation_input_tokens":usage.cache_creation_tokens,
        "cache_read_input_tokens":usage.cache_read_tokens,
    })
}

fn openai_finish(reason: Option<FinishReasonV1>) -> Value {
    reason
        .map(|reason| match reason {
            FinishReasonV1::Stop => "stop",
            FinishReasonV1::Length => "length",
            FinishReasonV1::ToolCalls => "tool_calls",
            FinishReasonV1::ContentFilter => "content_filter",
            FinishReasonV1::FunctionCall => "function_call",
            FinishReasonV1::Unknown => "unknown",
        })
        .map(|value| Value::String(value.into()))
        .unwrap_or(Value::Null)
}

fn anthropic_finish(reason: Option<FinishReasonV1>) -> Value {
    reason
        .map(|reason| match reason {
            FinishReasonV1::Stop => "end_turn",
            FinishReasonV1::Length => "max_tokens",
            FinishReasonV1::ToolCalls | FinishReasonV1::FunctionCall => "tool_use",
            FinishReasonV1::ContentFilter | FinishReasonV1::Unknown => "end_turn",
        })
        .map(|value| Value::String(value.into()))
        .unwrap_or(Value::Null)
}

fn content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn content_from_parts(parts: Vec<ContentPart>) -> Option<MessageContent> {
    match parts.as_slice() {
        [] => None,
        [ContentPart::Text { text }] => Some(MessageContent::Text(text.clone())),
        _ => Some(MessageContent::Parts(parts)),
    }
}

fn decode_stop(value: Option<&Value>) -> Result<Option<Vec<String>>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(stop)) => Ok(Some(vec![stop.clone()])),
        Some(Value::Array(stops)) => stops
            .iter()
            .map(|stop| {
                stop.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "stop entries must be strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err("stop must be a string or array".into()),
    }
}

fn required<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a Value, String> {
    object
        .get(key)
        .ok_or_else(|| format!("missing required field: {key}"))
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    required(object, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn typed_nonretryable_errors_suppress_status_based_sdk_retries_in_every_dialect() {
        for dialect in [
            IngressDialect::OpenAi,
            IngressDialect::Anthropic,
            IngressDialect::Responses,
            IngressDialect::Gemini,
        ] {
            for full in [false, true] {
                for error in [
                    sandhi_providers::ProviderError::OutputProjection(
                        "synthetic opaque state".into(),
                    ),
                    sandhi_providers::ProviderError::Auth,
                ] {
                    let response =
                        IngressError::from_provider_detail(&error, "fixture", full).render(dialect);
                    assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
                    assert_eq!(response.headers()["x-should-retry"], "false");
                    let bytes = axum::body::to_bytes(response.into_body(), 10_000)
                        .await
                        .unwrap();
                    if !full {
                        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic opaque state"));
                    }
                }
            }
            for error in [
                sandhi_providers::ProviderError::Transport("transient".into()),
                sandhi_providers::ProviderError::RateLimited,
            ] {
                let response =
                    IngressError::from_provider_redacted(&error, "fixture").render(dialect);
                assert!(!response.headers().contains_key("x-should-retry"));
            }
        }
    }
    use sandhi_core::AssistantOutputV1;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn openai_ingress_lifts_w3d_params_to_typed_fields() {
        // W3d/G7: promoted params must reach the typed fields at ingress, not
        // just survive inside the extensions body clone (which dies cross-family).
        let (request, _) = decode_openai_request(
            json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "hi"}],
                "reasoning_effort": "high",
                "thinking": {"type": "enabled", "budget_tokens": 2048}
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
        let thinking = request.thinking.expect("thinking lifted");
        assert!(thinking.enabled);
        assert_eq!(thinking.budget_tokens, Some(2048));
    }

    #[test]
    fn anthropic_stream_encoder_maintains_the_block_lifecycle() {
        // The Anthropic stream is a content-BLOCK protocol: unique indices, an
        // explicit start/stop per block, thinking and text as distinct block
        // types, and a spec-shaped ping. Pinned end to end over a
        // thinking→text→tool sequence.
        use sandhi_core::{ChatStreamEventV1 as E, UsageV2};

        let mut encoder = StreamEventEncoder::new(IngressDialect::Anthropic);
        let mut frames: Vec<(Option<&'static str>, Value)> = Vec::new();
        let events = [
            E::ResponseStart {
                id: Some("msg_1".into()),
                model: "claude-test".into(),
            },
            E::ReasoningDelta {
                delta: "thinking...".into(),
            },
            E::TextDelta {
                delta: "answer".into(),
            },
            E::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "lookup".into(),
            },
            E::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"q\":".into(),
            },
            E::ToolCallArgumentsDelta {
                index: 0,
                delta: "1}".into(),
            },
            E::ToolCallEnd { index: 0 },
            E::Usage {
                usage: UsageV2::default(),
            },
            E::Finish {
                reason: sandhi_core::FinishReasonV1::ToolCalls,
            },
        ];
        for event in &events {
            frames.extend(encoder.encode(event, None).unwrap());
        }
        let names: Vec<&str> = frames.iter().map(|(name, _)| name.unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "ping",
                "content_block_start", // 0: thinking
                "content_block_delta",
                "content_block_stop",
                "content_block_start", // 1: text
                "content_block_delta",
                "content_block_stop",
                "content_block_start", // 2: tool_use
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta", // usage
                "message_delta", // stop_reason
                "message_stop",
            ]
        );
        let block_starts: Vec<Value> = frames
            .iter()
            .filter(|(name, _)| *name == Some("content_block_start"))
            .map(|(_, value)| value.clone())
            .collect();
        assert_eq!(block_starts[0]["index"], 0);
        assert_eq!(block_starts[0]["content_block"]["type"], "thinking");
        assert_eq!(block_starts[1]["index"], 1);
        assert_eq!(block_starts[1]["content_block"]["type"], "text");
        assert_eq!(block_starts[2]["index"], 2);
        assert_eq!(block_starts[2]["content_block"]["type"], "tool_use");
        // Deltas carry their block's index (tool args deltas target block 2).
        let tool_deltas: Vec<&Value> = frames
            .iter()
            .filter(|(name, value)| {
                *name == Some("content_block_delta") && value["delta"]["type"] == "input_json_delta"
            })
            .map(|(_, value)| value)
            .collect();
        assert_eq!(tool_deltas.len(), 2);
        assert!(tool_deltas.iter().all(|d| d["index"] == 2));
        // The terminal message_delta carries the tool_use stop reason.
        let stop = frames
            .iter()
            .find(|(name, value)| {
                *name == Some("message_delta") && value["delta"]["stop_reason"].is_string()
            })
            .map(|(_, value)| value.clone())
            .unwrap();
        assert_eq!(stop["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn gemini_stream_encoder_assembles_whole_function_calls() {
        // Gemini has no partial tool-call frame: one complete functionCall part
        // per chunk. Text flows immediately; the call is buffered across
        // start/argument deltas and emitted complete at end; errors render as
        // an error frame instead of vanishing.
        use sandhi_core::{ChatStreamEventV1 as E, ProviderErrorV1};

        let mut encoder = StreamEventEncoder::new(IngressDialect::Gemini);
        let mut out: Vec<Value> = Vec::new();
        for event in [
            E::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "lookup".into(),
            },
            E::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"q\":".into(),
            },
            E::TextDelta {
                delta: "working".into(),
            },
            E::ToolCallArgumentsDelta {
                index: 0,
                delta: "1}".into(),
            },
            E::ToolCallEnd { index: 0 },
        ] {
            out.extend(
                encoder
                    .encode(&event, None)
                    .unwrap()
                    .into_iter()
                    .map(|(_, v)| v),
            );
        }
        // Text passed through immediately; the call arrived as ONE complete part.
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0]["candidates"][0]["content"]["parts"][0]["text"],
            "working"
        );
        let call = &out[1]["candidates"][0]["content"]["parts"][0]["functionCall"];
        assert_eq!(call["name"], "lookup");
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["args"]["q"], 1);

        let error_frames = encoder
            .encode(
                &E::Error {
                    error: ProviderErrorV1 {
                        code: "upstream".into(),
                        message: "boom".into(),
                        retryable: false,
                        http_status: None,
                        provider: None,
                        request_id: None,
                        details: BTreeMap::new(),
                    },
                },
                None,
            )
            .unwrap();
        assert_eq!(error_frames.len(), 1, "errors must render, not vanish");
        assert_eq!(error_frames[0].1["error"]["code"], 502);
        assert_eq!(error_frames[0].1["error"]["message"], "boom");
        assert!(error_frames[0].1["error"]["status"].is_string());
    }

    #[test]
    fn responses_stream_encoder_folds_output_into_the_terminal_frame() {
        // Codex-style clients re-read the final `response.output`; the terminal
        // `response.completed` frame must carry the assembled items (text
        // message + complete function calls), and `output_item.done` the full
        // item — not stubs.
        use sandhi_core::{ChatStreamEventV1 as E, FinishReasonV1, UsageV2};

        let mut encoder = StreamEventEncoder::new(IngressDialect::Responses);
        let mut frames: Vec<(Option<&'static str>, Value)> = Vec::new();
        let events = [
            E::ResponseStart {
                id: Some("resp_1".into()),
                model: "gpt-test".into(),
            },
            E::TextDelta {
                delta: "hel".into(),
            },
            E::TextDelta { delta: "lo".into() },
            E::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "lookup".into(),
            },
            E::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"q\":".into(),
            },
            E::ToolCallArgumentsDelta {
                index: 0,
                delta: "1}".into(),
            },
            E::ToolCallEnd { index: 0 },
            E::Finish {
                reason: FinishReasonV1::ToolCalls,
            },
        ];
        for event in &events {
            frames.extend(encoder.encode(event, Some(&UsageV2::default())).unwrap());
        }
        let added = frames
            .iter()
            .find(|(name, value)| {
                *name == Some("response.output_item.added")
                    && value["item"]["type"] == "function_call"
            })
            .map(|(_, value)| value.clone())
            .expect("output_item.added frame");
        // Text opened first (index 0), so the call lives at output_index 1 —
        // and added/done/fold must agree on that single index space.
        assert_eq!(added["output_index"], 1);
        let done_item = frames
            .iter()
            .find(|(name, value)| {
                *name == Some("response.output_item.done")
                    && value["item"]["type"] == "function_call"
            })
            .map(|(_, value)| value.clone())
            .expect("output_item.done frame");
        assert_eq!(done_item["output_index"], added["output_index"]);
        assert_eq!(done_item["item"]["type"], "function_call");
        assert_eq!(done_item["item"]["call_id"], "call_1");
        assert_eq!(done_item["item"]["name"], "lookup");
        assert_eq!(done_item["item"]["arguments"], "{\"q\":1}");

        let completed = frames
            .iter()
            .find(|(name, _)| *name == Some("response.completed"))
            .map(|(_, value)| value.clone())
            .expect("response.completed frame");
        let output = &completed["response"]["output"];
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["type"], "output_text");
        assert_eq!(output[0]["content"][0]["text"], "hello");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "call_1");
        assert_eq!(output[1]["arguments"], "{\"q\":1}");
        assert_eq!(output[1]["status"], "completed");
        assert_eq!(completed["response"]["status"], "completed");
    }

    #[test]
    fn openai_ingress_admits_guarded_native_extensions_passthrough() {
        // Third-party OpenAI-compat clients exercise family-native request extras
        // (e.g. Anthropic `cache_control`, provider `top_k`) through the reserved
        // `sandhi_extensions` body field. Buckets merge into the typed extensions
        // — where every family encoder inserts the typed fields AFTER cloning its
        // bucket, so a passthrough can only ADD native extras, never override the
        // decoded contract. The field itself never rides in the openai body clone.
        let (request, _) = decode_openai_request(
            json!({
                "model": "claude-test",
                "messages": [{"role": "user", "content": "hi"}],
                "sandhi_extensions": {"anthropic": {"top_k": 5}}
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert_eq!(request.extensions["anthropic"]["top_k"], json!(5));
        assert!(
            request.extensions["openai"]
                .get("sandhi_extensions")
                .is_none(),
            "the reserved field must not leak into the decoded openai body clone"
        );
    }

    #[test]
    fn openai_ingress_rejects_passthrough_overriding_the_decode_owned_bucket() {
        let error = decode_openai_request(
            json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "hi"}],
                "sandhi_extensions": {"openai": {"temperature": 0}}
            }),
            RequestMetadataV1::default(),
        )
        .unwrap_err();
        assert!(error.contains("openai"), "unexpected error: {error}");
    }

    #[test]
    fn openai_to_anthropic_translation_keeps_prompt_caching_alive() {
        // The Issue this ADR exists for, pinned end to end: a plain third-party
        // OpenAI-compat request (no `cache_control` anywhere — the OpenAI body
        // has no such field) decoded and re-encoded for an Anthropic upstream
        // must still carry cache markers on the wire, and a client using the
        // passthrough keeps marker placement in its own hands.
        use sandhi_providers::typed::encode_anthropic_request;

        let (decoded, _) = decode_openai_request(
            json!({
                "model": "claude-test",
                "max_tokens": 512,
                "messages": [
                    {"role": "system", "content": "policy"},
                    {"role": "user", "content": "hi"}
                ],
                "tools": [
                    {"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}},
                    {"type": "function", "function": {"name": "probe", "parameters": {"type": "object"}}}
                ]
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        let body = encode_anthropic_request(&decoded).unwrap();
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");

        // Passthrough alternative: the client places its own markers (here on
        // the first tool only) and the auto placement must stand down.
        let (decoded, _) = decode_openai_request(
            json!({
                "model": "claude-test",
                "max_tokens": 512,
                "messages": [
                    {"role": "system", "content": "policy"},
                    {"role": "user", "content": "hi"}
                ],
                "tools": [
                    {"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}},
                    {"type": "function", "function": {"name": "probe", "parameters": {"type": "object"}}}
                ],
                "sandhi_extensions": {"anthropic": {
                    "tools": [{"name": "lookup", "cache_control": {"type": "ephemeral"}}]
                }}
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        let body = encode_anthropic_request(&decoded).unwrap();
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert!(
            body["tools"][1].get("cache_control").is_none(),
            "client bucket present: auto placement must not add markers"
        );
    }

    #[test]
    fn strip_reserved_passthrough_rewrites_only_when_the_key_is_top_level() {
        use bytes::Bytes;

        let with_key = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "hi"}],
                "sandhi_extensions": {"anthropic": {"top_k": 5}}
            }))
            .unwrap(),
        );
        let stripped = strip_reserved_passthrough(with_key.clone());
        assert_ne!(stripped, with_key, "the reserved key must be removed");
        let value: Value = serde_json::from_slice(&stripped).unwrap();
        assert!(value.get("sandhi_extensions").is_none());
        assert_eq!(value["model"], "gpt-5");

        // Absent key and mention inside message content: byte-identical.
        let plain = Bytes::from_static(br#"{"model":"gpt-5","messages":[]}"#);
        assert_eq!(strip_reserved_passthrough(plain.clone()), plain);
        let nested = Bytes::from(
            serde_json::to_vec(&json!({
                "model":"gpt-5",
                "messages":[{"role":"user","content":"sandhi_extensions is not a top-level key"}]
            }))
            .unwrap(),
        );
        assert_eq!(strip_reserved_passthrough(nested.clone()), nested);

        // Non-object JSON containing the literal: unchanged.
        let array = Bytes::from_static(br#"["sandhi_extensions"]"#);
        assert_eq!(strip_reserved_passthrough(array.clone()), array);

        // A unicode-escaped spelling of the reserved key (hand-crafted; no
        // standard serializer escapes ASCII) still parses to the same key and
        // must be stripped — the `\u` presence check drives the parse.
        let escaped = Bytes::from_static(
            concat!(
                r#"{"model":"gpt-5","#,
                "\"\\u0073andhi_extensions\":{\"a\":1}}"
            )
            .as_bytes(),
        );
        let stripped = strip_reserved_passthrough(escaped.clone());
        assert_ne!(stripped, escaped, "escaped spelling must not survive");
        let value: Value = serde_json::from_slice(&stripped).unwrap();
        assert!(value
            .as_object()
            .unwrap()
            .get("sandhi_extensions")
            .is_none());

        // Escaped non-ASCII elsewhere with no reserved key: byte-identical
        // (the parse finds nothing to remove).
        let escaped_text = Bytes::from_static(
            "{\"model\":\"gpt-5\",\"messages\":[{\"role\":\"user\",\"content\":\"\u{4f60}\u{597d}\"}]}".as_bytes(),
        );
        assert_eq!(
            strip_reserved_passthrough(escaped_text.clone()),
            escaped_text
        );
    }

    #[test]
    fn anthropic_and_responses_decodes_drop_the_reserved_key_from_body_clones() {
        // ADR-0013: only the OpenAI dialect interprets `sandhi_extensions`;
        // the other decoders must not let it ride their family body clone (the
        // translation plane's re-encode base) to an upstream.
        let (request, _) = decode_anthropic_request(
            json!({
                "model": "claude-test",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}],
                "sandhi_extensions": {"anthropic": {"top_k": 5}}
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(request.extensions["anthropic"]
            .get("sandhi_extensions")
            .is_none());

        let (request, _) = decode_responses_request(
            json!({
                "model": "gpt-5",
                "input": "hi",
                "sandhi_extensions": {"anthropic": {"top_k": 5}}
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(request.extensions["openai_responses"]
            .get("sandhi_extensions")
            .is_none());

        // The Gemini decoder copies every unmodeled top-level key into its
        // body clone — the reserved key must be excluded so a forced
        // translation re-encode never carries it to the Gemini upstream.
        let (request, _) = decode_gemini_request(
            json!({
                "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT"}],
                "sandhi_extensions": {"anthropic": {"top_k": 5}}
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(request.extensions["gemini"]
            .get("sandhi_extensions")
            .is_none());
    }

    #[test]
    fn openai_ingress_validates_passthrough_shapes() {
        for bad in [
            json!("nope"),
            json!({"anthropic": "nope"}),
            json!({"Anthropic": {}}),
            json!({"anthropic": {"deep": {"nested": 1}}, "bad key!": {}}),
        ] {
            let error = decode_openai_request(
                json!({
                    "model": "gpt-5",
                    "messages": [{"role": "user", "content": "hi"}],
                    "sandhi_extensions": bad
                }),
                RequestMetadataV1::default(),
            )
            .unwrap_err();
            assert!(
                error.contains("sandhi_extensions"),
                "unexpected error: {error}"
            );
        }
    }

    #[test]
    fn gemini_ingress_lifts_thinking_budget() {
        for key in ["thinkingBudget", "thinking_budget"] {
            let (request, _) = decode_gemini_request(
                json!({
                    "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                    "generationConfig": {"thinkingConfig": {key: 0}}
                }),
                RequestMetadataV1::default(),
            )
            .unwrap();
            let thinking = request.thinking.expect("thinking lifted");
            assert!(!thinking.enabled); // budget 0 = disabled
        }
        assert!(decode_gemini_request(json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingBudget":0,"thinking_budget":1}}}),RequestMetadataV1::default()).unwrap_err().contains("aliases"));
    }

    #[test]
    fn gemini_unrepresented_thinking_controls_stay_native_instead_of_disappearing() {
        for config in [
            json!({"thinkingBudget":-1}),
            json!({"thinking_budget":-1}),
            json!({"thinkingBudget":0,"includeThoughts":true}),
            json!({"thinkingLevel":"high"}),
        ] {
            let (request, _) = decode_gemini_request(
                json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}],
                "generationConfig":{"maxOutputTokens":64,"thinkingConfig":config}}),
                RequestMetadataV1::default(),
            )
            .unwrap();
            assert!(request
                .native_input
                .as_ref()
                .unwrap()
                .nonportable_features
                .contains(&sandhi_core::NativeInputFeatureV1::NativeControl));
            assert!(request
                .native_input_for(sandhi_core::EndpointFamilyV1::AnthropicMessages)
                .is_err());
            assert!(request
                .native_input_for(sandhi_core::EndpointFamilyV1::GeminiGenerateContent)
                .is_ok());
        }
    }

    #[test]
    fn anthropic_dialect_accepts_the_sdks_x_api_key_header() {
        // The stock Anthropic SDK authenticates with `x-api-key` and nothing else.
        assert_eq!(
            IngressDialect::Anthropic.extract_credential(&headers(&[("x-api-key", "vk_demo")])),
            Some("vk_demo")
        );
    }

    #[test]
    fn anthropic_dialect_still_accepts_bearer_and_prefers_the_native_header() {
        assert_eq!(
            IngressDialect::Anthropic
                .extract_credential(&headers(&[("authorization", "Bearer vk_bearer")])),
            Some("vk_bearer")
        );
        assert_eq!(
            IngressDialect::Anthropic.extract_credential(&headers(&[
                ("authorization", "Bearer vk_bearer"),
                ("x-api-key", "vk_native"),
            ])),
            Some("vk_native")
        );
    }

    #[test]
    fn openai_dialects_accept_bearer_only() {
        for dialect in [IngressDialect::OpenAi, IngressDialect::Responses] {
            assert_eq!(
                dialect.extract_credential(&headers(&[("authorization", "Bearer vk_demo")])),
                Some("vk_demo")
            );
            // `x-api-key` is not an OpenAI credential scheme — accepting it would invent one.
            assert_eq!(
                dialect.extract_credential(&headers(&[("x-api-key", "vk_demo")])),
                None
            );
        }
    }

    #[test]
    fn missing_or_malformed_credentials_extract_to_none() {
        for dialect in [
            IngressDialect::OpenAi,
            IngressDialect::Responses,
            IngressDialect::Anthropic,
        ] {
            assert_eq!(dialect.extract_credential(&HeaderMap::new()), None);
            // Wrong scheme, empty token, and an empty native header all fail closed.
            assert_eq!(
                dialect.extract_credential(&headers(&[("authorization", "Basic vk_demo")])),
                None
            );
            assert_eq!(
                dialect.extract_credential(&headers(&[("authorization", "Bearer   ")])),
                None
            );
            assert_eq!(
                dialect.extract_credential(&headers(&[
                    ("x-api-key", "   "),
                    ("authorization", "Bearer vk_demo"),
                ])),
                Some("vk_demo")
            );
        }
    }

    #[test]
    fn openai_ingress_covers_all_roles_and_tool_linkage() {
        let (request, stream) = decode_request(
            IngressDialect::OpenAi,
            json!({
                "model":"m","stream":true,
                "messages":[
                    {"role":"developer","content":"d"},
                    {"role":"system","content":"s"},
                    {"role":"user","content":[{"type":"text","text":"u"}]},
                    {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
                    {"role":"tool","tool_call_id":"c1","content":"ok"},
                    {"role":"function","name":"f","content":"legacy"}
                ]
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(stream);
        assert_eq!(request.messages.len(), 6);
        request.validate().unwrap();
    }

    #[test]
    fn anthropic_ingress_maps_tool_use_and_result() {
        let (request, _) = decode_request(
            IngressDialect::Anthropic,
            json!({
                "model":"claude","max_tokens":10,"system":"safe",
                "messages":[
                    {"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"f","input":{"x":1}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"ok"}]}
                ]
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(matches!(
            request.messages[1],
            ChatMessageV1::Assistant { .. }
        ));
        assert!(matches!(request.messages[2], ChatMessageV1::Tool { .. }));
    }

    #[test]
    fn both_egress_dialects_preserve_cache_split() {
        let response = ChatResponseV1 {
            schema_version: CHAT_SCHEMA_VERSION_V1.into(),
            id: Some("r".into()),
            model: "m".into(),
            output: AssistantOutputV1 {
                content: Some(MessageContent::Text("ok".into())),
                tool_calls: Vec::new(),
                refusal: None,
            },
            finish_reason: Some(FinishReasonV1::Stop),
            usage: UsageV2 {
                tokens_in: 6,
                tokens_out: 5,
                cache_read_tokens: 4,
                ..UsageV2::default()
            },
            extensions: BTreeMap::new(),
        };
        assert_eq!(
            encode_response(IngressDialect::OpenAi, &response).unwrap()["usage"]["prompt_tokens"],
            10
        );
        assert_eq!(
            encode_response(IngressDialect::Anthropic, &response).unwrap()["usage"]
                ["cache_read_input_tokens"],
            4
        );
    }

    #[test]
    fn responses_ingress_maps_instructions_items_and_tools() {
        let (request, stream) = decode_request(
            IngressDialect::Responses,
            json!({
                "model":"gpt-test","stream":true,
                "instructions":"be precise",
                "input":[
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"weather?"}]},
                    {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Austin\"}"},
                    {"type":"function_call_output","call_id":"call_1","output":"sunny"}
                ],
                "tools":[{"type":"function","name":"weather","parameters":{"type":"object"}}],
                "tool_choice":{"type":"function","name":"weather"},
                "temperature":0.5,
                "max_output_tokens":100
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(stream);
        // instructions → System, user message, assistant tool call, tool output.
        assert_eq!(request.messages.len(), 4);
        assert!(matches!(request.messages[0], ChatMessageV1::System { .. }));
        assert!(matches!(request.messages[1], ChatMessageV1::User { .. }));
        assert!(matches!(
            request.messages[2],
            ChatMessageV1::Assistant { ref tool_calls, .. } if tool_calls.len() == 1
                && tool_calls[0].id == "call_1"
                && tool_calls[0].name == "weather"
        ));
        assert!(matches!(
            request.messages[3],
            ChatMessageV1::Tool { ref tool_call_id, .. } if tool_call_id == "call_1"
        ));
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "weather");
        assert!(matches!(
            request.tool_choice,
            Some(ToolChoiceV1::Function { ref name }) if name == "weather"
        ));
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.max_output_tokens, Some(100));
        // The raw body is retained for a Responses-backend upstream.
        assert!(request.extensions.contains_key("openai_responses"));
        request.validate().unwrap();
    }

    #[test]
    fn responses_ingress_rejects_missing_model() {
        let err = decode_request(
            IngressDialect::Responses,
            json!({"input":"hi"}),
            RequestMetadataV1::default(),
        )
        .unwrap_err();
        assert!(err.contains("model"));
    }

    #[test]
    fn responses_shorthand_history_survives_translation() {
        for stream in [false, true] {
            let shorthand = json!({
                "model":"gpt-test","stream":stream,"max_output_tokens":64,
                "input":[
                    {"role":"system","content":"be precise"},
                    {"role":"developer","content":"preserve history"},
                    {"role":"user","content":"first question"},
                    {"role":"assistant","content":[{"type":"output_text","text":"first answer"}]},
                    {"role":"user","content":[{"type":"input_text","text":"next question"}]}
                ]
            });
            let mut explicit = shorthand.clone();
            for item in explicit["input"].as_array_mut().unwrap() {
                item["type"] = json!("message");
            }
            let (request, wants_stream) = decode_request(
                IngressDialect::Responses,
                shorthand,
                RequestMetadataV1::default(),
            )
            .unwrap();
            let (expected, _) = decode_request(
                IngressDialect::Responses,
                explicit,
                RequestMetadataV1::default(),
            )
            .unwrap();
            assert_eq!(wants_stream, stream);
            assert_eq!(request.messages, expected.messages);
            let wire = sandhi_providers::typed::encode_openai_request(&request).unwrap();
            assert_eq!(wire["messages"].as_array().unwrap().len(), 5);
            assert_eq!(wire["messages"][2]["content"], "first question");
            assert_eq!(wire["messages"][4]["content"], "next question");
        }
    }

    #[test]
    fn translated_cache_write_usage_round_trips_without_double_counting() {
        let usage = UsageV2 {
            tokens_in: 10,
            tokens_out: 20,
            cache_read_tokens: 60,
            cache_creation_tokens: 30,
            ..UsageV2::default()
        };
        for (wire, parse) in [
            (
                openai_usage(&usage),
                sandhi_core::parse_openai_usage as fn(&Value) -> Option<sandhi_core::ParsedUsage>,
            ),
            (
                responses_usage(&usage),
                sandhi_core::parse_openai_responses_usage
                    as fn(&Value) -> Option<sandhi_core::ParsedUsage>,
            ),
        ] {
            let parsed = parse(&json!({"usage":wire})).unwrap();
            assert_eq!(parsed.tokens_in, 10);
            assert_eq!(parsed.cache_read_tokens, 60);
            assert_eq!(parsed.cache_creation_tokens, 30);
            assert_eq!(sandhi_core::billable(&parsed.into()), 120);
        }
        // Existing payloads without measured writes retain their previous shape.
        for wire in [
            openai_usage(&UsageV2::default()),
            responses_usage(&UsageV2::default()),
        ] {
            let details = wire
                .get("prompt_tokens_details")
                .or_else(|| wire.get("input_tokens_details"))
                .unwrap();
            assert!(details.get("cache_write_tokens").is_none());
        }
    }

    #[test]
    fn responses_egress_shapes_message_function_call_and_usage() {
        let response = ChatResponseV1 {
            schema_version: CHAT_SCHEMA_VERSION_V1.into(),
            id: Some("resp_1".into()),
            model: "gpt-test".into(),
            output: AssistantOutputV1 {
                content: Some(MessageContent::Text("hi".into())),
                tool_calls: vec![ToolCallV1 {
                    id: "call_1".into(),
                    name: "weather".into(),
                    arguments: "{\"city\":\"Austin\"}".into(),
                    extensions: BTreeMap::new(),
                }],
                refusal: None,
            },
            finish_reason: Some(FinishReasonV1::ToolCalls),
            usage: UsageV2 {
                tokens_in: 40,
                tokens_out: 20,
                cache_read_tokens: 60,
                reasoning_tokens: Some(7),
                ..UsageV2::default()
            },
            extensions: BTreeMap::new(),
        };
        let body = encode_response(IngressDialect::Responses, &response).unwrap();
        assert_eq!(body["object"], "response");
        assert_eq!(body["id"], "resp_1");
        assert_eq!(body["status"], "completed");
        // First output item is the assistant message with output_text; second is the function_call.
        assert_eq!(body["output"][0]["type"], "message");
        assert_eq!(body["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(body["output"][0]["content"][0]["text"], "hi");
        assert_eq!(body["output"][1]["type"], "function_call");
        assert_eq!(body["output"][1]["call_id"], "call_1");
        assert_eq!(body["output"][1]["name"], "weather");
        assert_eq!(body["usage"]["input_tokens"], 100);
        assert_eq!(body["usage"]["output_tokens"], 20);
        assert_eq!(body["usage"]["input_tokens_details"]["cached_tokens"], 60);
        assert_eq!(
            body["usage"]["output_tokens_details"]["reasoning_tokens"],
            7
        );
    }

    #[test]
    fn responses_egress_marks_length_as_incomplete() {
        let response = ChatResponseV1 {
            schema_version: CHAT_SCHEMA_VERSION_V1.into(),
            id: None,
            model: "m".into(),
            output: AssistantOutputV1 {
                content: Some(MessageContent::Text("...".into())),
                tool_calls: Vec::new(),
                refusal: None,
            },
            finish_reason: Some(FinishReasonV1::Length),
            usage: UsageV2::default(),
            extensions: BTreeMap::new(),
        };
        let body = encode_response(IngressDialect::Responses, &response).unwrap();
        assert_eq!(body["status"], "incomplete");
        assert_eq!(body["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn responses_stream_events_fold_terminal_usage_into_completed() {
        let mut encoder = StreamEventEncoder::new(IngressDialect::Responses);
        let start = encoder
            .encode(
                &ChatStreamEventV1::ResponseStart {
                    id: Some("resp_1".into()),
                    model: "gpt-test".into(),
                },
                None,
            )
            .unwrap();
        assert_eq!(start[0].0, Some("response.created"));
        assert_eq!(start[0].1["response"]["model"], "gpt-test");
        let delta = encoder
            .encode(&ChatStreamEventV1::TextDelta { delta: "hi".into() }, None)
            .unwrap();
        assert!(delta
            .iter()
            .any(|(kind, value)| *kind == Some("response.output_text.delta")
                && value["delta"] == "hi"));
        let usage = UsageV2 {
            tokens_in: 5,
            tokens_out: 2,
            ..UsageV2::default()
        };
        assert!(encoder
            .encode(
                &ChatStreamEventV1::Usage {
                    usage: usage.clone()
                },
                None
            )
            .unwrap()
            .is_empty());
        let finish = encoder
            .encode(
                &ChatStreamEventV1::Finish {
                    reason: FinishReasonV1::Stop,
                },
                Some(&usage),
            )
            .unwrap();
        let (kind, terminal) = finish.last().unwrap();
        assert_eq!(*kind, Some("response.completed"));
        assert_eq!(terminal["response"]["status"], "completed");
        assert_eq!(terminal["response"]["usage"]["input_tokens"], 5);
        assert_eq!(terminal["response"]["usage"]["output_tokens"], 2);
        assert_eq!(terminal["response"]["usage"]["total_tokens"], 7);
    }

    #[test]
    fn streamed_refusal_and_filter_semantics_are_preserved_or_explicitly_refused() {
        for dialect in [
            IngressDialect::OpenAi,
            IngressDialect::Responses,
            IngressDialect::Anthropic,
            IngressDialect::Gemini,
        ] {
            let mut encoder = StreamEventEncoder::new(dialect);
            let refusal = encoder.encode(
                &ChatStreamEventV1::RefusalDelta {
                    delta: "declined".into(),
                },
                None,
            );
            assert_eq!(
                refusal.is_ok(),
                matches!(dialect, IngressDialect::OpenAi | IngressDialect::Responses)
            );
            let mut encoder = StreamEventEncoder::new(dialect);
            let terminal = encoder.encode(
                &ChatStreamEventV1::Finish {
                    reason: FinishReasonV1::ContentFilter,
                },
                None,
            );
            assert_eq!(terminal.is_ok(), dialect != IngressDialect::Anthropic);
        }
    }
}

#[cfg(test)]
mod gemini_codec_tests {
    use super::*;
    use sandhi_core::AssistantOutputV1;

    fn metadata() -> RequestMetadataV1 {
        RequestMetadataV1 {
            session_id: None,
            virtual_key_id: None,
            subject_id: None,
            group_id: None,
            route: None,
            idempotency_key: None,
            run_id: None,
            step_id: None,
            parent_id: None,
            trace_context: None,
        }
    }

    /// TD-0010 D4b's central claim: this decode is the MIRROR of the adapter's request encoder.
    ///
    /// Executing that encoder here is not possible — every `*_typed` codec in `sandhi-providers`
    /// is a private module, a boundary worth more than this test's convenience. So the mirror is
    /// pinned from BOTH sides instead: `gemini_typed`'s own tests assert the wire shape it emits
    /// (e.g. `systemInstruction.parts[0].text`), and this consumes exactly that shape and asserts
    /// every field arrives. If either side moves, one of the two fails.
    #[test]
    fn decodes_the_exact_wire_shape_the_adapter_emits() {
        // Byte-for-byte the structure `gemini_typed::encode_gemini_request` produces.
        let wire = json!({
            "contents": [
                {"role":"user","parts":[{"text":"ping"}]},
                {"role":"model","parts":[{"text":"pong"}]}
            ],
            "systemInstruction": {"parts":[{"text":"be terse"}]},
            "tools": [{"functionDeclarations":[{
                "name":"lookup",
                "description":"look something up",
                "parameters":{"type":"object","properties":{"q":{"type":"string"}}}
            }]}],
            "toolConfig": {"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["lookup"]}},
            "generationConfig": {"temperature":0.25,"maxOutputTokens":512,"stopSequences":["END"]}
        });

        let (request, streaming) =
            decode_gemini_request(wire, metadata()).expect("the mirror decodes the adapter shape");
        assert!(!streaming, "streaming is a path verb, never a body field");

        // Every field the encoder writes must arrive — a field lost here is a field silently
        // dropped from a caller's cross-family request.
        assert_eq!(request.messages.len(), 3, "system + user + model");
        assert!(matches!(
            &request.messages[0],
            ChatMessageV1::System { content: MessageContent::Text(t), .. } if t == "be terse"
        ));
        assert!(matches!(
            &request.messages[2],
            ChatMessageV1::Assistant { content: Some(MessageContent::Text(t)), .. } if t == "pong"
        ));
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "lookup");
        assert_eq!(
            request.tool_choice,
            Some(ToolChoiceV1::Function {
                name: "lookup".into()
            })
        );
        assert_eq!(request.temperature, Some(0.25));
        assert_eq!(request.max_output_tokens, Some(512));
        assert_eq!(request.stop.as_deref(), Some(&["END".to_string()][..]));
    }

    #[test]
    fn inline_media_and_tool_results_survive_the_decode() {
        let body = json!({
            "contents": [
                {"role":"user","parts":[
                    {"text":"what is this?"},
                    {"inlineData":{"mimeType":"image/png","data":"aGk="}}
                ]},
                {"role":"model","parts":[{"functionCall":{"name":"lookup","id":"call_1","args":{"q":"x"}}}]},
                {"role":"user","parts":[{"functionResponse":{"name":"lookup","id":"call_1","response":{"output":"found"}}}]}
            ]
        });
        let (request, _) = decode_gemini_request(body, metadata()).unwrap();

        // Mixed text + image keeps its parts rather than flattening to text.
        match &request.messages[0] {
            ChatMessageV1::User {
                content: MessageContent::Parts(parts),
                ..
            } => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[1], ContentPart::ImageUrl { .. }));
            }
            other => panic!("expected a multi-part user message, got {other:?}"),
        }
        // A functionCall becomes an assistant tool call, arguments preserved as JSON text.
        match &request.messages[1] {
            ChatMessageV1::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "lookup");
                assert_eq!(tool_calls[0].id, "call_1");
                assert!(tool_calls[0].arguments.contains("\"q\""));
            }
            other => panic!("expected an assistant tool call, got {other:?}"),
        }
        // A functionResponse is a TOOL result, not a user turn.
        match &request.messages[2] {
            ChatMessageV1::Tool { tool_call_id, .. } => assert_eq!(tool_call_id, "call_1"),
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    #[test]
    fn gemini_inline_sdk_mime_alias_and_urlsafe_bytes_are_portable() {
        let body = json!({"contents":[{"role":"user","parts":[
            {"inlineData":{"mime_type":"image/png","data":"-_8"}}
        ]}]});
        let (request, _) = decode_gemini_request(body, metadata()).unwrap();
        let ChatMessageV1::User {
            content: MessageContent::Parts(parts),
            ..
        } = &request.messages[0]
        else {
            panic!("inline image must retain its content part");
        };
        assert!(matches!(&parts[0], ContentPart::ImageUrl { image_url, .. }
            if image_url == "data:image/png;base64,+/8="));
        assert!(request
            .native_input_for(sandhi_core::EndpointFamilyV1::OpenaiChatCompletions)
            .is_ok());
    }

    #[test]
    fn gemini_inline_conflicting_mime_and_invalid_bytes_refuse() {
        for inline in [
            json!({"mimeType":"image/png","mime_type":"audio/wav","data":"AA=="}),
            json!({"mimeType":4,"data":"AA=="}),
            json!({"mimeType":"image/png","data":"invalid?"}),
            json!({"mimeType":"image/png","data":4}),
            json!({"mime_type":"image//png","data":"AA=="}),
        ] {
            let body = json!({"contents":[{"role":"user","parts":[{"inlineData":inline}]}]});
            assert!(decode_gemini_request(body, metadata()).is_err(), "{inline}");
        }
    }

    #[test]
    fn unmodelled_gemini_fields_are_preserved_not_dropped() {
        let body = json!({
            "contents": [{"role":"user","parts":[{"text":"hi"}]}],
            "safetySettings": [{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"BLOCK_NONE"}],
            "cachedContent": "caches/abc",
            "generationConfig": {"temperature": 0.5, "topP": 0.9, "responseMimeType": "application/json"}
        });
        let (request, _) = decode_gemini_request(body, metadata()).unwrap();

        assert_eq!(request.temperature, Some(0.5));
        // W3d/G7 D5: unmodelled fields survive under `extensions["gemini"]` —
        // the key the gemini encoder reads — so they round-trip losslessly.
        let gemini = request.extensions["gemini"].as_object().unwrap();
        assert!(gemini.contains_key("safetySettings"));
        assert!(gemini.contains_key("cachedContent"));
        assert_eq!(gemini["generationConfig"]["topP"], json!(0.9));
        assert_eq!(
            gemini["generationConfig"]["responseMimeType"],
            json!("application/json")
        );
    }

    #[test]
    fn gemini_ingress_stash_matches_the_encoder_base_key() {
        // W3d/G7 D5: the encoder (gemini_typed::encode_gemini_request) clones
        // extensions["gemini"] as its base body and its generationConfig from
        // extensions["gemini"]["generationConfig"]. The ingress stash must land
        // exactly there for a gemini->typed->gemini round-trip to be lossless —
        // pre-D5 it landed at extensions["topP"]/extensions["generationConfig"],
        // which the encoder never read (silent drop on re-encode).
        let body = json!({
            "contents": [{"role":"user","parts":[{"text":"hi"}]}],
            "safetySettings": [{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"BLOCK_NONE"}],
            "generationConfig": {"temperature": 0.5, "topP": 0.9}
        });
        let (request, _) = decode_gemini_request(body, metadata()).unwrap();

        // The ONLY extensions key is "gemini" — no stray flat keys the encoder can't see.
        assert_eq!(
            request.extensions.keys().collect::<Vec<_>>(),
            vec!["gemini"]
        );
        let gemini = request.extensions["gemini"].as_object().unwrap();
        assert_eq!(
            gemini["safetySettings"][0]["threshold"],
            json!("BLOCK_NONE")
        );
        // temperature is lifted to a typed field; topP (unmodelled) stays in the base gen-config.
        assert_eq!(gemini["generationConfig"]["topP"], json!(0.9));
        assert!(gemini["generationConfig"].get("temperature").is_none());
    }

    #[test]
    fn responses_are_rendered_in_geminis_shape() {
        let response = ChatResponseV1 {
            schema_version: CHAT_SCHEMA_VERSION_V1.to_string(),
            id: Some("resp_1".into()),
            model: "gpt-mock".into(),
            output: AssistantOutputV1 {
                content: Some(MessageContent::Text("pong".into())),
                tool_calls: vec![ToolCallV1 {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: "{\"q\":\"x\"}".into(),
                    extensions: BTreeMap::new(),
                }],
                refusal: None,
            },
            finish_reason: Some(FinishReasonV1::ToolCalls),
            usage: UsageV2 {
                tokens_in: 11,
                tokens_out: 3,
                cache_read_tokens: 4,
                ..UsageV2::default()
            },
            extensions: BTreeMap::new(),
        };
        let encoded = encode_response(IngressDialect::Gemini, &response).unwrap();

        assert_eq!(
            encoded["candidates"][0]["content"]["parts"][0]["text"],
            "pong"
        );
        // Gemini carries tool arguments as an OBJECT, not the OpenAI-family JSON string.
        assert_eq!(
            encoded["candidates"][0]["content"]["parts"][1]["functionCall"]["args"]["q"],
            "x"
        );
        assert_eq!(encoded["usageMetadata"]["promptTokenCount"], 15);
        assert_eq!(encoded["usageMetadata"]["cachedContentTokenCount"], 4);
        // A tool turn is an ordinary STOP for Gemini; the functionCall part is the signal.
        assert_eq!(encoded["candidates"][0]["finishReason"], "STOP");
    }
}
