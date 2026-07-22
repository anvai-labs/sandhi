//! Stable wire facts for known OpenAI-compatible providers.
//!
//! This is deliberately not a model/capability catalog. Sandhi owns transport facts
//! (canonical slug, aliases, endpoint routing); consumers such as Victor own model
//! policy, tool selection, context budgeting, and user-facing discovery.

use sandhi_core::{
    EndpointFamilyV1, ModelDescriptorV1, ProviderCapabilitiesV1, ProviderDescriptorV1,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelEndpointRoute {
    pub model_prefix: &'static str,
    pub base_url: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenAiCompatProviderSpec {
    pub slug: &'static str,
    pub aliases: &'static [&'static str],
    pub base_url: &'static str,
    pub model_routes: &'static [ModelEndpointRoute],
    /// Host option name → provider HTTP header. Sandhi owns the wire spelling; hosts provide
    /// only values through the typed runtime's explicit header map.
    pub header_options: &'static [(&'static str, &'static str)],
    /// Named endpoint option → base URL for providers with region/plan-specific routing.
    pub endpoint_options: &'static [(&'static str, &'static str)],
}

impl OpenAiCompatProviderSpec {
    #[must_use]
    pub fn base_url_for_model(&self, model: &str) -> &'static str {
        self.model_routes
            .iter()
            .find(|route| model.starts_with(route.model_prefix))
            .map_or(self.base_url, |route| route.base_url)
    }
}

const MOONSHOT_ROUTES: &[ModelEndpointRoute] = &[ModelEndpointRoute {
    model_prefix: "kimi-k3",
    base_url: "https://api.moonshot.ai/v1",
}];

pub const OPENAI_COMPAT_PROVIDER_SPECS: &[OpenAiCompatProviderSpec] = &[
    OpenAiCompatProviderSpec {
        slug: "openai",
        aliases: &[],
        base_url: "https://api.openai.com/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "moonshot",
        aliases: &["kimi"],
        base_url: "https://api.moonshot.cn/v1",
        model_routes: MOONSHOT_ROUTES,
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "together",
        aliases: &[],
        base_url: "https://api.together.xyz/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "groq",
        aliases: &["groqcloud"],
        base_url: "https://api.groq.com/openai/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "cerebras",
        aliases: &[],
        base_url: "https://api.cerebras.ai/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "fireworks",
        aliases: &[],
        base_url: "https://api.fireworks.ai/inference/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "openrouter",
        aliases: &[],
        base_url: "https://openrouter.ai/api/v1",
        model_routes: &[],
        header_options: &[("site_url", "HTTP-Referer"), ("site_name", "X-Title")],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "xai",
        aliases: &["grok"],
        base_url: "https://api.x.ai/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "mistral",
        aliases: &[],
        base_url: "https://api.mistral.ai/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "deepseek",
        aliases: &[],
        base_url: "https://api.deepseek.com/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[],
    },
    OpenAiCompatProviderSpec {
        slug: "zai",
        aliases: &[
            "zhipu",
            "zhipuai",
            "zai-coding-plan",
            "zai-coding",
            "glm-coding",
        ],
        base_url: "https://api.z.ai/api/paas/v4",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[
            ("standard", "https://api.z.ai/api/paas/v4"),
            ("coding", "https://api.z.ai/api/coding/paas/v4"),
            ("china", "https://open.bigmodel.cn/api/paas/v4"),
            (
                "china-coding",
                "https://open.bigmodel.cn/api/coding/paas/v4",
            ),
        ],
    },
    OpenAiCompatProviderSpec {
        slug: "qwen",
        aliases: &["dashscope", "alibaba"],
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        model_routes: &[],
        header_options: &[],
        endpoint_options: &[
            (
                "standard",
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
            ),
            ("portal", "https://portal.qwen.ai/v1"),
            ("coding", "https://coding.dashscope.aliyuncs.com/v1"),
        ],
    },
];

#[must_use]
pub fn resolve_openai_compat_provider(name: &str) -> Option<&'static OpenAiCompatProviderSpec> {
    let normalized = name.trim().to_ascii_lowercase();
    OPENAI_COMPAT_PROVIDER_SPECS.iter().find(|spec| {
        spec.slug == normalized || spec.aliases.iter().any(|alias| *alias == normalized)
    })
}

#[must_use]
pub fn openai_compat_descriptor(name: &str) -> Option<ProviderDescriptorV1> {
    let spec = resolve_openai_compat_provider(name)?;
    let models = if spec.slug == "moonshot" {
        vec![ModelDescriptorV1 {
            id: "kimi-k3".into(),
            aliases: Vec::new(),
            max_input_tokens: Some(1_048_576),
            max_output_tokens: None,
            default_temperature: Some(1.0),
            capabilities: ProviderCapabilitiesV1 {
                streaming: true,
                tools: true,
                parallel_tool_calls: true,
                vision: true,
                reasoning: true,
                ..ProviderCapabilitiesV1::default()
            },
            endpoint_url: Some("https://api.moonshot.ai/v1".into()),
            extensions: BTreeMap::from([(
                "reasoning_effort_values".into(),
                serde_json::json!(["low", "high", "max"]),
            )]),
        }]
    } else {
        Vec::new()
    };
    Some(ProviderDescriptorV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        slug: spec.slug.into(),
        aliases: spec.aliases.iter().map(|alias| (*alias).into()).collect(),
        endpoint_family: EndpointFamilyV1::OpenaiChatCompletions,
        base_url: spec.base_url.into(),
        capabilities: ProviderCapabilitiesV1 {
            streaming: true,
            tools: true,
            parallel_tool_calls: true,
            prompt_cache_usage: true,
            ..ProviderCapabilitiesV1::default()
        },
        // Models remain empty until a stable model descriptor is explicitly admitted. Sandhi
        // must not manufacture volatile capabilities merely because a provider shares a wire.
        models,
        extensions: BTreeMap::from([
            (
                "header_options".into(),
                serde_json::Value::Object(
                    spec.header_options
                        .iter()
                        .map(|(option, header)| {
                            (
                                (*option).into(),
                                serde_json::Value::String((*header).into()),
                            )
                        })
                        .collect(),
                ),
            ),
            (
                "endpoint_options".into(),
                serde_json::Value::Object(
                    spec.endpoint_options
                        .iter()
                        .map(|(option, base_url)| {
                            (
                                (*option).into(),
                                serde_json::Value::String((*base_url).into()),
                            )
                        })
                        .collect(),
                ),
            ),
        ]),
    })
}

/// Resolve every provider family supported by the typed runtime. This is the single catalog
/// surface used by bindings and gateway discovery; consumers must not duplicate endpoint facts.
#[must_use]
pub fn provider_descriptor(name: &str) -> Option<ProviderDescriptorV1> {
    if let Some(descriptor) = openai_compat_descriptor(name) {
        return Some(descriptor);
    }

    let normalized = name.trim().to_ascii_lowercase();
    let (slug, aliases, endpoint_family, base_url, capabilities) = match normalized.as_str() {
        "anthropic" | "claude" => (
            "anthropic",
            &["claude"][..],
            EndpointFamilyV1::AnthropicMessages,
            "https://api.anthropic.com",
            ProviderCapabilitiesV1 {
                streaming: true,
                tools: true,
                parallel_tool_calls: true,
                vision: true,
                prompt_cache_usage: true,
                reasoning: true,
                ..ProviderCapabilitiesV1::default()
            },
        ),
        "gemini" | "google" => (
            "gemini",
            &["google"][..],
            EndpointFamilyV1::GeminiGenerateContent,
            "https://generativelanguage.googleapis.com/v1beta",
            ProviderCapabilitiesV1 {
                streaming: true,
                tools: true,
                parallel_tool_calls: true,
                vision: true,
                audio_input: true,
                file_input: true,
                structured_output: true,
                reasoning: true,
                prompt_cache_usage: true,
            },
        ),
        "cohere" => (
            "cohere",
            &[][..],
            EndpointFamilyV1::CohereChat,
            "https://api.cohere.com",
            ProviderCapabilitiesV1 {
                streaming: true,
                tools: true,
                parallel_tool_calls: true,
                vision: true,
                prompt_cache_usage: true,
                ..ProviderCapabilitiesV1::default()
            },
        ),
        "ollama" => (
            "ollama",
            &[][..],
            EndpointFamilyV1::OllamaChat,
            "http://localhost:11434",
            ProviderCapabilitiesV1 {
                streaming: true,
                tools: true,
                parallel_tool_calls: true,
                vision: true,
                structured_output: true,
                ..ProviderCapabilitiesV1::default()
            },
        ),
        _ => return None,
    };

    Some(ProviderDescriptorV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        slug: slug.into(),
        aliases: aliases.iter().map(|alias| (*alias).into()).collect(),
        endpoint_family,
        base_url: base_url.into(),
        capabilities,
        models: Vec::new(),
        extensions: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_aliases_to_canonical_slugs() {
        assert_eq!(
            resolve_openai_compat_provider("kimi").map(|spec| spec.slug),
            Some("moonshot")
        );
        assert_eq!(
            resolve_openai_compat_provider("groq").map(|spec| spec.slug),
            Some("groq")
        );
    }

    #[test]
    fn moonshot_routes_models_without_duplicating_adapters() {
        let spec = resolve_openai_compat_provider("moonshot").unwrap();
        assert_eq!(
            spec.base_url_for_model("kimi-k3"),
            "https://api.moonshot.ai/v1"
        );
        assert_eq!(
            spec.base_url_for_model("kimi-k2-thinking"),
            "https://api.moonshot.cn/v1"
        );
        let descriptor = openai_compat_descriptor("kimi").unwrap();
        assert_eq!(descriptor.models[0].default_temperature, Some(1.0));
        assert_eq!(
            descriptor.models[0].extensions["reasoning_effort_values"],
            serde_json::json!(["low", "high", "max"])
        );
    }

    #[test]
    fn descriptor_exposes_typed_wire_capabilities_without_inventing_models() {
        let descriptor = openai_compat_descriptor("grok").unwrap();
        assert_eq!(descriptor.slug, "xai");
        assert_eq!(
            descriptor.endpoint_family,
            EndpointFamilyV1::OpenaiChatCompletions
        );
        assert!(descriptor.capabilities.streaming);
        assert!(descriptor.models.is_empty());
    }

    #[test]
    fn provider_specific_header_names_are_owned_by_the_wire_catalog() {
        let descriptor = openai_compat_descriptor("openrouter").unwrap();
        assert_eq!(
            descriptor.extensions["header_options"]["site_url"],
            "HTTP-Referer"
        );
        assert_eq!(
            descriptor.extensions["header_options"]["site_name"],
            "X-Title"
        );
    }

    #[test]
    fn named_endpoints_and_new_admitted_aliases_are_owned_by_the_catalog() {
        let zai = provider_descriptor("zhipu").unwrap();
        assert_eq!(zai.slug, "zai");
        assert_eq!(
            zai.extensions["endpoint_options"]["coding"],
            "https://api.z.ai/api/coding/paas/v4"
        );
        assert_eq!(provider_descriptor("deepseek").unwrap().slug, "deepseek");
        assert_eq!(provider_descriptor("dashscope").unwrap().slug, "qwen");
        assert_eq!(provider_descriptor("alibaba").unwrap().slug, "qwen");
        assert_eq!(provider_descriptor("zhipuai").unwrap().slug, "zai");
        assert_eq!(provider_descriptor("zai-coding-plan").unwrap().slug, "zai");
    }

    #[test]
    fn native_descriptors_and_aliases_share_the_catalog() {
        let anthropic = provider_descriptor("claude").unwrap();
        assert_eq!(anthropic.slug, "anthropic");
        assert_eq!(
            anthropic.endpoint_family,
            EndpointFamilyV1::AnthropicMessages
        );
        assert!(anthropic.capabilities.tools);

        let gemini = provider_descriptor("google").unwrap();
        assert_eq!(gemini.slug, "gemini");
        assert!(gemini.capabilities.audio_input);
    }
}
