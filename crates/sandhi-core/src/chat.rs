//! Versioned, provider-neutral chat contract shared by all Sandhi front doors.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const CHAT_SCHEMA_VERSION_V1: &str = "1";

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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
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
}
