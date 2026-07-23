//! Chat Completions message-role contract.
//!
//! Roles are wire facts, so Sandhi validates them before any request is sent.
//! Model/provider policy (for example whether a particular compatible backend
//! accepts the newer `developer` role) remains with the caller.

use crate::ProviderError;
use serde_json::Value;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiChatRole {
    Developer,
    System,
    User,
    Assistant,
    Tool,
    /// Legacy function-result role. New integrations should use [`Self::Tool`].
    Function,
}

impl OpenAiChatRole {
    pub const ALL: [Self; 6] = [
        Self::Developer,
        Self::System,
        Self::User,
        Self::Assistant,
        Self::Tool,
        Self::Function,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Developer => "developer",
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Function => "function",
        }
    }
}

impl FromStr for OpenAiChatRole {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "developer" => Ok(Self::Developer),
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            "function" => Ok(Self::Function),
            _ => Err(format!(
                "unsupported Chat Completions message role {value:?}"
            )),
        }
    }
}

/// Validate role-specific linkage without rewriting the caller's message body.
///
/// A missing `messages` field is left to the upstream endpoint so the generic
/// adapter can still carry provider-specific operations and test probes.
pub fn validate_openai_chat_messages(body: &Value) -> Result<(), ProviderError> {
    let Some(messages) = body.get("messages") else {
        return Ok(());
    };
    let messages = messages.as_array().ok_or_else(|| {
        ProviderError::InvalidRequest("messages must be a JSON array".to_string())
    })?;

    for (index, message) in messages.iter().enumerate() {
        let object = message.as_object().ok_or_else(|| {
            ProviderError::InvalidRequest(format!("messages[{index}] must be an object"))
        })?;
        let role_text = object.get("role").and_then(Value::as_str).ok_or_else(|| {
            ProviderError::InvalidRequest(format!("messages[{index}].role must be a string"))
        })?;
        let role = OpenAiChatRole::from_str(role_text).map_err(|reason| {
            ProviderError::InvalidRequest(format!("messages[{index}]: {reason}"))
        })?;

        let required_nonempty_string = |field: &str| {
            object
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        };
        match role {
            OpenAiChatRole::Tool if !required_nonempty_string("tool_call_id") => {
                return Err(ProviderError::InvalidRequest(format!(
                    "messages[{index}] with role=tool requires tool_call_id"
                )));
            }
            OpenAiChatRole::Function if !required_nonempty_string("name") => {
                return Err(ProviderError::InvalidRequest(format!(
                    "messages[{index}] with role=function requires name"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_every_current_and_legacy_chat_role() {
        let body = json!({"messages": [
            {"role": "developer", "content": "policy"},
            {"role": "system", "content": "compat policy"},
            {"role": "user", "content": "question"},
            {"role": "assistant", "content": null, "tool_calls": []},
            {"role": "tool", "content": "result", "tool_call_id": "call_1"},
            {"role": "function", "content": "legacy result", "name": "lookup"}
        ]});
        validate_openai_chat_messages(&body).unwrap();
        assert_eq!(
            OpenAiChatRole::ALL.map(OpenAiChatRole::as_str),
            [
                "developer",
                "system",
                "user",
                "assistant",
                "tool",
                "function"
            ]
        );
    }

    #[test]
    fn rejects_unknown_or_unlinked_roles_before_the_wire() {
        let unknown = json!({"messages": [{"role": "model", "content": "x"}]});
        assert!(matches!(
            validate_openai_chat_messages(&unknown),
            Err(ProviderError::InvalidRequest(_))
        ));

        let unlinked_tool = json!({"messages": [{"role": "tool", "content": "x"}]});
        assert!(matches!(
            validate_openai_chat_messages(&unlinked_tool),
            Err(ProviderError::InvalidRequest(_))
        ));
    }
}
