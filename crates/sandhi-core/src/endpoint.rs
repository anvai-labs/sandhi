//! Non-secret endpoint conventions shared by configuration and embedded callers.
use crate::EndpointFamilyV1;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointProfileV1 {
    #[default]
    Standard,
    #[serde(rename = "chatgpt_codex")]
    ChatGptCodex,
    AnthropicImplicitCache,
    OpenaiTerminalChunkUsage,
    ZaiFinalChoiceUsage,
    DeepseekFinalChoiceUsage,
}

impl EndpointProfileV1 {
    pub const fn openai_stream_usage_profile(self) -> crate::usage::OpenAiStreamUsageProfile {
        use crate::usage::OpenAiStreamUsageProfile as P;
        match self {
            Self::OpenaiTerminalChunkUsage => P::TerminalChunk,
            Self::ZaiFinalChoiceUsage => P::ZaiFinalChoice,
            Self::DeepseekFinalChoiceUsage => P::DeepSeekFinalChoice,
            _ => P::Standard,
        }
    }

    /// This is only a protocol compatibility check, never credential authority.
    pub fn validate_family(self, family: EndpointFamilyV1) -> Result<(), &'static str> {
        match self {
            Self::Standard => Ok(()),
            Self::ChatGptCodex if family == EndpointFamilyV1::OpenaiResponses => Ok(()),
            Self::ChatGptCodex => Err("Codex profile requires OpenAI Responses"),
            Self::AnthropicImplicitCache if family == EndpointFamilyV1::AnthropicMessages => Ok(()),
            Self::AnthropicImplicitCache => {
                Err("implicit Messages usage profile requires Anthropic Messages")
            }
            Self::OpenaiTerminalChunkUsage
            | Self::ZaiFinalChoiceUsage
            | Self::DeepseekFinalChoiceUsage
                if family == EndpointFamilyV1::OpenaiChatCompletions =>
            {
                Ok(())
            }
            Self::OpenaiTerminalChunkUsage
            | Self::ZaiFinalChoiceUsage
            | Self::DeepseekFinalChoiceUsage => {
                Err("terminal Chat usage profile requires OpenAI Chat Completions")
            }
        }
    }
}

impl std::str::FromStr for EndpointProfileV1 {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > 64 {
            return Err("unsupported endpoint_profile");
        }
        match value.trim().to_ascii_lowercase().as_str() {
            "standard" => Ok(Self::Standard),
            "chatgpt_codex" => Ok(Self::ChatGptCodex),
            "anthropic_implicit_cache" => Ok(Self::AnthropicImplicitCache),
            "openai_terminal_chunk_usage" => Ok(Self::OpenaiTerminalChunkUsage),
            "zai_final_choice_usage" => Ok(Self::ZaiFinalChoiceUsage),
            "deepseek_final_choice_usage" => Ok(Self::DeepseekFinalChoiceUsage),
            _ => Err("unsupported endpoint_profile"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stored_profile_names_keep_their_existing_values() {
        for (profile, wire) in [
            (EndpointProfileV1::Standard, "standard"),
            (
                EndpointProfileV1::ZaiFinalChoiceUsage,
                "zai_final_choice_usage",
            ),
            (
                EndpointProfileV1::DeepseekFinalChoiceUsage,
                "deepseek_final_choice_usage",
            ),
            (EndpointProfileV1::ChatGptCodex, "chatgpt_codex"),
            (
                EndpointProfileV1::AnthropicImplicitCache,
                "anthropic_implicit_cache",
            ),
            (
                EndpointProfileV1::OpenaiTerminalChunkUsage,
                "openai_terminal_chunk_usage",
            ),
        ] {
            let encoded = serde_json::to_value(profile).unwrap();
            assert_eq!(encoded, wire);
            assert_eq!(
                serde_json::from_value::<EndpointProfileV1>(encoded).unwrap(),
                profile
            );
            assert_eq!(wire.parse::<EndpointProfileV1>().unwrap(), profile);
        }
    }
    #[test]
    fn vendor_profiles_and_peer_conventions_preserve_the_same_contract() {
        for (profile, convention) in [
            (
                EndpointProfileV1::ZaiFinalChoiceUsage,
                crate::UsageConventionV1::ChatZaiFinalChoice,
            ),
            (
                EndpointProfileV1::DeepseekFinalChoiceUsage,
                crate::UsageConventionV1::ChatDeepseekFinalChoice,
            ),
        ] {
            assert_eq!(
                profile.openai_stream_usage_profile(),
                convention.chat_profile()
            );
            assert_eq!(
                convention
                    .wire_value()
                    .parse::<crate::UsageConventionV1>()
                    .unwrap(),
                convention
            );
            assert!(profile.validate_family(convention.family()).is_ok());
            assert!(profile
                .validate_family(EndpointFamilyV1::OpenaiResponses)
                .is_err());
        }
    }
}
