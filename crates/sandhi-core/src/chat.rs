//! Versioned, provider-neutral chat contract shared by all Sandhi front doors.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const CHAT_SCHEMA_VERSION_V1: &str = "1";

/// Additive growth counter within v1 (W3c). Bumped whenever the contract
/// gains fields — machine-enforced by `contract_minor_ratchets_with_schema_
/// changes`, which fails when the rendered schemas change without a bump.
/// History: 1 = usage latency/reasoning on UsageEvent (#68), 2 =
/// `include_native_response` request gate (#90), 3 = wire-truth latency on
/// `UsageV2` (#97). Consumers feature-detect the binding export and treat an
/// absent fn as minor 0.
pub const CHAT_CONTRACT_MINOR: u32 = 3;

fn schema_v1() -> String {
    CHAT_SCHEMA_VERSION_V1.to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    InputAudio {
        data: String,
        format: String,
    },
    File {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallV1 {
    pub id: String,
    pub name: String,
    /// JSON object encoded as a string, retained incrementally and losslessly across streams.
    pub arguments: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessageV1 {
    Developer {
        content: MessageContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    System {
        content: MessageContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    User {
        content: MessageContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<MessageContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCallV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
    },
    Tool {
        content: MessageContent,
        tool_call_id: String,
    },
    /// Legacy Chat Completions function result; retained for lossless compatibility.
    Function {
        content: MessageContent,
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolDefinitionV1 {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    None,
    Auto,
    Required,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ToolChoiceV1 {
    Mode(ToolChoiceMode),
    Function { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct RequestMetadataV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    // --- ADR-0005 D7 neutral identity (attribution metadata, never pricing) ---
    /// Caller-supplied key for at-most-once semantics across retries of one logical call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Agent-run identifier; groups every call one run makes (cost-tree root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Step within a run (plan/act/verify…); child dimension under `run_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Parent step/run for nested agents, so an agent's cost tree is reconstructable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// W3C `traceparent` value, linking the call into distributed traces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChatRequestV1 {
    #[serde(default = "schema_v1")]
    pub schema_version: String,
    pub model: String,
    pub messages: Vec<ChatMessageV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinitionV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default)]
    pub metadata: RequestMetadataV1,
    /// Whether adapters re-embed the full native provider response body under
    /// `extensions[<endpoint family>]` on the response. Defaults to true for
    /// compatibility; clients that consume only the neutral contract can opt
    /// out to shed bandwidth and coupling (G8). The `reasoning` extension is
    /// unaffected — it is decoded content, not a native body.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub include_native_response: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

fn default_true() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(value: &bool) -> bool {
    *value
}

impl ChatRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CHAT_SCHEMA_VERSION_V1 {
            return Err(format!(
                "unsupported chat schema version: {}",
                self.schema_version
            ));
        }
        if self.model.trim().is_empty() {
            return Err("model must not be empty".into());
        }
        for message in &self.messages {
            match message {
                ChatMessageV1::Tool { tool_call_id, .. } if tool_call_id.trim().is_empty() => {
                    return Err("tool message requires a non-empty tool_call_id".into());
                }
                ChatMessageV1::Function { name, .. } if name.trim().is_empty() => {
                    return Err("function message requires a non-empty name".into());
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum UsageCompleteness {
    Final,
    Partial,
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct UsageV2 {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_prediction_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_prediction_tokens: Option<u64>,
    #[serde(default)]
    pub completeness: UsageCompleteness,
    #[serde(default = "one")]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
    /// Wall-clock call duration measured at the typed boundary (W3b) — wire
    /// truth, so clients stop self-measuring around the FFI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Time from request dispatch to the first streamed event (streaming
    /// only; W3b). Absent on non-streaming calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_to_first_token_ms: Option<u64>,
}

const fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FinishReasonV1 {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AssistantOutputV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ChatResponseV1 {
    #[serde(default = "schema_v1")]
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model: String,
    pub output: AssistantOutputV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReasonV1>,
    pub usage: UsageV2,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderErrorV1 {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndpointFamilyV1 {
    OpenaiChatCompletions,
    OpenaiResponses,
    AnthropicMessages,
    GeminiGenerateContent,
    CohereChat,
    OllamaChat,
    BedrockConverse,
    HostCallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
pub struct ProviderCapabilitiesV1 {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub audio_input: bool,
    #[serde(default)]
    pub file_input: bool,
    #[serde(default)]
    pub structured_output: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub prompt_cache_usage: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelDescriptorV1 {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_temperature: Option<f64>,
    #[serde(default)]
    pub capabilities: ProviderCapabilitiesV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderDescriptorV1 {
    #[serde(default = "schema_v1")]
    pub schema_version: String,
    pub slug: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub endpoint_family: EndpointFamilyV1,
    pub base_url: String,
    #[serde(default)]
    pub capabilities: ProviderCapabilitiesV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDescriptorV1>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ChatStreamEventV1 {
    ResponseStart {
        id: Option<String>,
        model: String,
    },
    TextDelta {
        delta: String,
    },
    ReasoningDelta {
        delta: String,
    },
    RefusalDelta {
        delta: String,
    },
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        index: u32,
        delta: String,
    },
    ToolCallEnd {
        index: u32,
    },
    Usage {
        usage: UsageV2,
    },
    Finish {
        reason: FinishReasonV1,
    },
    Error {
        error: ProviderErrorV1,
    },
}

/// Derive a `session_id` from standard signals when no explicit one is present (ADR-0005 D7).
///
/// A drop-in OpenAI/Anthropic SDK cannot set a custom `x-sandhi-session` header, but cache
/// affinity still wants a stable per-conversation key. Precedence:
///
/// 1. `explicit` — an operator-supplied value (header or `metadata.session_id`) always wins.
/// 2. OpenAI `user` (top-level string on the request body).
/// 3. Anthropic `metadata.user_id`.
/// 4. A stable FNV-1a hash of the request's system+tools prefix — the cacheable prefix itself
///    is the affinity key of last resort. `None` when the body carries none of these.
///
/// Neutral metadata only; the derived value is an affinity/attribution key, never an identity
/// assertion (self-reported inputs — ADR-0005 D7 trust language).
#[must_use]
pub fn derive_session_id(explicit: Option<&str>, body: &Value) -> Option<String> {
    if let Some(session) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(session.to_string());
    }
    if let Some(user) = body.get("user").and_then(Value::as_str) {
        let user = user.trim();
        if !user.is_empty() {
            return Some(user.to_string());
        }
    }
    if let Some(user_id) = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
    {
        let user_id = user_id.trim();
        if !user_id.is_empty() {
            return Some(user_id.to_string());
        }
    }
    // Prefix hash: system prompt(s) + tool definitions, serialized deterministically. Covers
    // both wire shapes — OpenAI (`messages[role=system|developer]`) and Anthropic (`system`).
    let mut prefix: Vec<Value> = Vec::new();
    if let Some(system) = body.get("system") {
        prefix.push(system.clone());
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            match message.get("role").and_then(Value::as_str) {
                Some("system" | "developer") => prefix.push(message.clone()),
                _ => break, // the cacheable prefix ends at the first non-system message
            }
        }
    }
    if let Some(tools) = body.get("tools") {
        prefix.push(tools.clone());
    }
    if prefix.is_empty() {
        return None;
    }
    let canonical = serde_json::to_string(&prefix).ok()?;
    Some(format!("prefix-{:016x}", fnv1a_64(canonical.as_bytes())))
}

/// FNV-1a 64-bit — a stable, dependency-free hash for affinity keys (not a security boundary).
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Whether a model id is safe to embed in a provider URL **path** (ADR-0004 D4 / TD-0006).
///
/// Gemini-style upstreams place the model inside the request path
/// (`/v1beta/models/{model}:generateContent`); an unvalidated id is a path-traversal vector.
/// Rejects empty/oversized ids, control chars, whitespace, path separators (`/`, `\`), `..`,
/// and URL metacharacters (`?`, `#`, `%`). Compat providers whose ids legitimately contain
/// `/` (openrouter, fireworks) never path-embed the model, so they do not call this.
#[must_use]
pub fn model_id_is_path_safe(model: &str) -> bool {
    if model.is_empty() || model.len() > 256 || model.contains("..") {
        return false;
    }
    model
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@'))
}

/// Serde tag census of [`ChatStreamEventV1`]. Deliberately an exhaustive `match`:
/// adding a variant fails compilation HERE, forcing the author through the
/// TD-0008 operating rule — every new event variant ships with a
/// consumer-decision row (consumed / surfaced / explicitly ignored) in
/// `docs/td/TD-0008-victor-codesign-boundary.md` and a consumer notification,
/// because producer-side capability with no consumer decision is how silent
/// gaps form (reasoning_delta, refusal_delta).
pub fn stream_event_variant_tag(event: &ChatStreamEventV1) -> &'static str {
    match event {
        ChatStreamEventV1::ResponseStart { .. } => "response_start",
        ChatStreamEventV1::TextDelta { .. } => "text_delta",
        ChatStreamEventV1::ReasoningDelta { .. } => "reasoning_delta",
        ChatStreamEventV1::RefusalDelta { .. } => "refusal_delta",
        ChatStreamEventV1::ToolCallStart { .. } => "tool_call_start",
        ChatStreamEventV1::ToolCallArgumentsDelta { .. } => "tool_call_arguments_delta",
        ChatStreamEventV1::ToolCallEnd { .. } => "tool_call_end",
        ChatStreamEventV1::Usage { .. } => "usage",
        ChatStreamEventV1::Finish { .. } => "finish",
        ChatStreamEventV1::Error { .. } => "error",
    }
}

/// Render the checked JSON Schema documents from the Rust source of truth.
#[must_use]
pub fn contract_schema_documents() -> BTreeMap<&'static str, String> {
    fn render(schema: &schemars::schema::RootSchema) -> String {
        let mut json = serde_json::to_string_pretty(schema).expect("RootSchema serializes");
        json.push('\n');
        json
    }

    BTreeMap::from([
        (
            "chat-request.v1.schema.json",
            render(&schemars::schema_for!(ChatRequestV1)),
        ),
        (
            "chat-response.v1.schema.json",
            render(&schemars::schema_for!(ChatResponseV1)),
        ),
        (
            "chat-stream-event.v1.schema.json",
            render(&schemars::schema_for!(ChatStreamEventV1)),
        ),
        (
            "usage-aggregate.v1.schema.json",
            render(&schemars::schema_for!(crate::stats::UsageAggregateV1)),
        ),
        (
            "provider-descriptor.v1.schema.json",
            render(&schemars::schema_for!(ProviderDescriptorV1)),
        ),
        (
            "provider-error.v1.schema.json",
            render(&schemars::schema_for!(ProviderErrorV1)),
        ),
        (
            "usage.v2.schema.json",
            render(&schemars::schema_for!(UsageV2)),
        ),
    ])
}

#[cfg(test)]
mod tests {
    #[test]
    fn usage_latency_fields_are_absent_by_default() {
        // Old usage JSON (fields absent) round-trips; absent stays absent so
        // pre-W3b consumers see byte-identical documents.
        let usage: UsageV2 = serde_json::from_value(serde_json::json!({
            "tokens_in": 1, "tokens_out": 2,
            "cache_creation_tokens": 0, "cache_read_tokens": 0
        }))
        .unwrap();
        assert!(usage.duration_ms.is_none());
        assert!(usage.time_to_first_token_ms.is_none());
        let wire = serde_json::to_value(&usage).unwrap();
        assert!(wire.get("duration_ms").is_none());
        assert!(wire.get("time_to_first_token_ms").is_none());

        let mut stamped = usage.clone();
        stamped.duration_ms = Some(120);
        stamped.time_to_first_token_ms = Some(45);
        let wire = serde_json::to_value(&stamped).unwrap();
        assert_eq!(wire["duration_ms"], 120);
        assert_eq!(wire["time_to_first_token_ms"], 45);
    }

    #[test]
    fn include_native_response_defaults_true_and_stays_off_the_wire() {
        // Old request JSON (field absent) keeps today's behavior: include.
        let req: ChatRequestV1 = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        assert!(req.include_native_response);

        // Default stays off the wire — old consumers see byte-identical requests.
        let wire = serde_json::to_value(&req).unwrap();
        assert!(wire.get("include_native_response").is_none());

        // Opt-out round-trips.
        let mut opted_out = req.clone();
        opted_out.include_native_response = false;
        let wire = serde_json::to_value(&opted_out).unwrap();
        assert_eq!(wire["include_native_response"], serde_json::json!(false));
        let back: ChatRequestV1 = serde_json::from_value(wire).unwrap();
        assert!(!back.include_native_response);
    }

    use super::*;

    #[test]
    fn all_roles_round_trip_and_assistant_content_can_be_absent() {
        let value = serde_json::json!({
            "model": "m",
            "messages": [
                {"role":"developer", "content":"d"},
                {"role":"system", "content":"s"},
                {"role":"user", "content":[{"type":"text", "text":"u"}]},
                {"role":"assistant", "tool_calls":[{"id":"c1","name":"lookup","arguments":"{}"}]},
                {"role":"tool", "content":"ok", "tool_call_id":"c1"},
                {"role":"function", "content":"legacy", "name":"lookup"}
            ]
        });
        let request: ChatRequestV1 = serde_json::from_value(value).unwrap();
        request.validate().unwrap();
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["messages"][3]["role"], "assistant");
        assert!(encoded["messages"][3].get("content").is_none());
    }

    #[test]
    fn invalid_role_linkage_is_rejected() {
        let request: ChatRequestV1 = serde_json::from_value(serde_json::json!({
            "model":"m", "messages":[{"role":"tool","content":"x","tool_call_id":""}]
        }))
        .unwrap();
        assert!(request.validate().unwrap_err().contains("tool_call_id"));
    }

    #[test]
    fn checked_contract_schemas_match_rust_types() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas");
        for (filename, generated) in contract_schema_documents() {
            let checked = std::fs::read_to_string(root.join(filename))
                .unwrap_or_else(|error| panic!("read checked schema {filename}: {error}"));
            assert_eq!(checked, generated, "regenerate {filename}");
        }
    }

    #[test]
    fn contract_minor_ratchets_with_schema_changes() {
        // Additive-only within v1 is policy (TD-0002); this makes the growth
        // signal machine-enforced: any change to the rendered contract
        // schemas must come with a CHAT_CONTRACT_MINOR bump and a digest
        // update here. Consumers gate feature trust on the minor.
        fn fnv1a(bytes: &[u8]) -> u64 {
            let mut hash: u64 = 0xcbf29ce484222325;
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash
        }
        let mut concatenated = String::new();
        for (filename, generated) in contract_schema_documents() {
            concatenated.push_str(filename);
            concatenated.push('\n');
            concatenated.push_str(&generated);
        }
        let digest = fnv1a(concatenated.as_bytes());
        assert_eq!(
            (CHAT_CONTRACT_MINOR, digest),
            (3, 0x46c44575425c789a_u64),
            "contract schemas changed: bump CHAT_CONTRACT_MINOR and update this digest \
             (new digest = {digest:#x})"
        );
    }

    #[test]
    fn metadata_identity_fields_are_additive_and_round_trip() {
        // Old wire shape (no identity fields) still deserializes (TD-0002 v1 policy).
        let legacy: RequestMetadataV1 =
            serde_json::from_value(serde_json::json!({ "session_id": "s1" })).unwrap();
        assert_eq!(legacy.run_id, None);
        // Present fields survive a round trip and absent ones stay off the wire.
        let metadata = RequestMetadataV1 {
            session_id: Some("s1".into()),
            run_id: Some("run-1".into()),
            step_id: Some("plan".into()),
            trace_context: Some("00-abc-def-01".into()),
            ..RequestMetadataV1::default()
        };
        let json = serde_json::to_string(&metadata).unwrap();
        assert!(!json.contains("idempotency_key"));
        assert!(!json.contains("parent_id"));
        let back: RequestMetadataV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(metadata, back);
    }

    #[test]
    fn session_id_derivation_precedence_and_stability() {
        let openai_body = serde_json::json!({
            "model": "gpt-5",
            "user": "team-billing-7",
            "messages": [{"role": "system", "content": "You are helpful."}]
        });
        // Explicit always wins.
        assert_eq!(
            derive_session_id(Some("explicit-1"), &openai_body).as_deref(),
            Some("explicit-1")
        );
        // OpenAI `user`.
        assert_eq!(
            derive_session_id(None, &openai_body).as_deref(),
            Some("team-billing-7")
        );
        // Anthropic `metadata.user_id`.
        let anthropic_body = serde_json::json!({
            "model": "claude-x",
            "metadata": {"user_id": "acct-42"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert_eq!(
            derive_session_id(None, &anthropic_body).as_deref(),
            Some("acct-42")
        );
        // Prefix hash of system+tools: stable across calls, changes when the prefix changes,
        // and ignores the volatile non-prefix suffix (the user turn).
        let prefixed = serde_json::json!({
            "model": "gpt-5",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "turn 1"}
            ],
            "tools": [{"name": "lookup"}]
        });
        let a = derive_session_id(None, &prefixed).unwrap();
        let b = derive_session_id(None, &prefixed).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("prefix-"));
        let mut next_turn = prefixed.clone();
        next_turn["messages"][1]["content"] = serde_json::json!("turn 2");
        assert_eq!(derive_session_id(None, &next_turn).unwrap(), a);
        let mut different = prefixed.clone();
        different["messages"][0]["content"] = serde_json::json!("You are terse.");
        assert_ne!(derive_session_id(None, &different).unwrap(), a);
        // No signal at all -> None.
        let bare = serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}]
        });
        assert_eq!(derive_session_id(None, &bare), None);
        // Whitespace-only explicit/user values do not shadow the fallback chain.
        assert_eq!(
            derive_session_id(Some("  "), &anthropic_body).as_deref(),
            Some("acct-42")
        );
    }

    #[test]
    fn model_path_safety_rejects_traversal_and_metacharacters() {
        for ok in [
            "gemini-3-pro",
            "gpt-5",
            "claude-sonnet-5",
            "model_1:latest",
            "m@2026.1",
        ] {
            assert!(model_id_is_path_safe(ok), "{ok} should be path-safe");
        }
        for bad in [
            "",
            "../secrets",
            "a/../b",
            "models/other",
            "a\\b",
            "a b",
            "a?x=1",
            "a#frag",
            "a%2e%2e",
            "a\u{7}b",
            &"m".repeat(257),
        ] {
            assert!(!model_id_is_path_safe(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn stream_event_census_matches_schema() {
        // The census list a consumer conformance suite mirrors (victor
        // tests/unit/providers/test_sandhi_event_conformance.py). If this
        // fails, a variant was added/renamed: update TD-0008's
        // consumer-decision table and the consumer suites in the same change.
        let expected = [
            "response_start",
            "text_delta",
            "reasoning_delta",
            "refusal_delta",
            "tool_call_start",
            "tool_call_arguments_delta",
            "tool_call_end",
            "usage",
            "finish",
            "error",
        ];
        let schema = contract_schema_documents()
            .remove("chat-stream-event.v1.schema.json")
            .expect("stream event schema present");
        for tag in expected {
            assert!(
                schema.contains(&format!("\"{tag}\"")),
                "census tag {tag:?} missing from the checked schema"
            );
        }
        assert_eq!(
            stream_event_variant_tag(&ChatStreamEventV1::TextDelta {
                delta: String::new()
            }),
            "text_delta"
        );
    }

    #[test]
    fn chat_and_usage_wire_versions_bump_together() {
        // The bindings' `wire_contract_version()` returns the usage-event
        // version, and consumers (victor `_verify_wire_contract`) validate
        // their CHAT expectation against it. That is only sound while the two
        // versions are equal. If this fails you are splitting them: export a
        // dedicated chat_contract_version from the bindings first and
        // coordinate consumers (TD-0008 operating rule 1).
        assert_eq!(CHAT_SCHEMA_VERSION_V1, crate::UsageEvent::SCHEMA_VERSION);
    }
}
