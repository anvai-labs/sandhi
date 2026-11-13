//! Immediate-peer role and wire usage conventions. Neither conveys credential authority.
use crate::{
    usage::{AnthropicUsageProfile, OpenAiStreamUsageProfile},
    EndpointFamilyV1,
};
use serde::{Deserialize, Serialize};

pub const USAGE_CONVENTION_HEADER: &str = "x-sandhi-usage-convention";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerRoleV1 {
    #[default]
    Provider,
    SandhiGatewayV1,
}
impl std::str::FromStr for PeerRoleV1 {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "provider" => Ok(Self::Provider),
            "sandhi_gateway_v1" => Ok(Self::SandhiGatewayV1),
            _ => Err("unsupported peer role"),
        }
    }
}

/// Counts interpretation only: deliberately has no Codex/authentication profile variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageConventionV1 {
    ChatStandard,
    ChatTerminal,
    ChatZaiFinalChoice,
    ChatDeepseekFinalChoice,
    MessagesStandard,
    MessagesImplicitCache,
    ResponsesStandard,
    GeminiStandard,
    CohereStandard,
    OllamaStandard,
}
impl UsageConventionV1 {
    pub const fn wire_value(self) -> &'static str {
        match self {
            Self::ChatStandard => "v1-chat-standard",
            Self::ChatTerminal => "v1-chat-terminal",
            Self::ChatZaiFinalChoice => "v1-chat-zai-final-choice",
            Self::ChatDeepseekFinalChoice => "v1-chat-deepseek-final-choice",
            Self::MessagesStandard => "v1-messages-standard",
            Self::MessagesImplicitCache => "v1-messages-implicit-cache",
            Self::ResponsesStandard => "v1-responses-standard",
            Self::GeminiStandard => "v1-gemini-standard",
            Self::CohereStandard => "v1-cohere-standard",
            Self::OllamaStandard => "v1-ollama-standard",
        }
    }
    pub const fn family(self) -> EndpointFamilyV1 {
        match self {
            Self::ChatStandard
            | Self::ChatTerminal
            | Self::ChatZaiFinalChoice
            | Self::ChatDeepseekFinalChoice => EndpointFamilyV1::OpenaiChatCompletions,
            Self::MessagesStandard | Self::MessagesImplicitCache => {
                EndpointFamilyV1::AnthropicMessages
            }
            Self::ResponsesStandard => EndpointFamilyV1::OpenaiResponses,
            Self::GeminiStandard => EndpointFamilyV1::GeminiGenerateContent,
            Self::CohereStandard => EndpointFamilyV1::CohereChat,
            Self::OllamaStandard => EndpointFamilyV1::OllamaChat,
        }
    }
    pub fn standard(family: EndpointFamilyV1) -> Result<Self, &'static str> {
        match family {
            EndpointFamilyV1::OpenaiChatCompletions => Ok(Self::ChatStandard),
            EndpointFamilyV1::AnthropicMessages => Ok(Self::MessagesStandard),
            EndpointFamilyV1::OpenaiResponses => Ok(Self::ResponsesStandard),
            EndpointFamilyV1::GeminiGenerateContent => Ok(Self::GeminiStandard),
            EndpointFamilyV1::CohereChat => Ok(Self::CohereStandard),
            EndpointFamilyV1::OllamaChat => Ok(Self::OllamaStandard),
            _ => Err("unsupported gateway client dialect"),
        }
    }
    pub const fn anthropic_profile(self) -> AnthropicUsageProfile {
        match self {
            Self::MessagesImplicitCache => AnthropicUsageProfile::ImplicitCache,
            _ => AnthropicUsageProfile::SeparateCacheWrites,
        }
    }
    pub const fn chat_profile(self) -> OpenAiStreamUsageProfile {
        match self {
            Self::ChatTerminal => OpenAiStreamUsageProfile::TerminalChunk,
            Self::ChatZaiFinalChoice => OpenAiStreamUsageProfile::ZaiFinalChoice,
            Self::ChatDeepseekFinalChoice => OpenAiStreamUsageProfile::DeepSeekFinalChoice,
            _ => OpenAiStreamUsageProfile::Standard,
        }
    }
}
impl std::str::FromStr for UsageConventionV1 {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "v1-chat-standard" => Ok(Self::ChatStandard),
            "v1-chat-terminal" => Ok(Self::ChatTerminal),
            "v1-chat-zai-final-choice" => Ok(Self::ChatZaiFinalChoice),
            "v1-chat-deepseek-final-choice" => Ok(Self::ChatDeepseekFinalChoice),
            "v1-messages-standard" => Ok(Self::MessagesStandard),
            "v1-messages-implicit-cache" => Ok(Self::MessagesImplicitCache),
            "v1-responses-standard" => Ok(Self::ResponsesStandard),
            "v1-gemini-standard" => Ok(Self::GeminiStandard),
            "v1-cohere-standard" => Ok(Self::CohereStandard),
            "v1-ollama-standard" => Ok(Self::OllamaStandard),
            _ => Err("unsupported gateway usage convention"),
        }
    }
}
