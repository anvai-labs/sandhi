//! Stable wire facts and curated model descriptors for known providers.
//!
//! Per TD-0004, Sandhi owns catalog **data** — curated, release-versioned model
//! descriptors (id, context window, max output, wire capabilities; **no pricing** —
//! the measure-vs-price line is held). Consumers such as Victor own catalog
//! **policy** (which models to expose/select, discovery UX) on top of this data.
//! Sandhi additionally owns transport facts (canonical slug, aliases, endpoint
//! routing). Admitting a provider's model data here does not advertise pricing or
//! dictate selection policy.

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
    let models = compat_models(spec.slug);
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

/// Curated Anthropic (Claude) model descriptors — catalog **data** (TD-0004).
///
/// Sourced from Anthropic's Models overview (platform.claude.com), current as of 2026-07.
/// Context windows and max output are stable facts; **no pricing** is carried (the
/// measure-vs-price line is held). Selection/exposure policy is the consumer's job.
#[must_use]
pub fn anthropic_models() -> Vec<ModelDescriptorV1> {
    let caps = ProviderCapabilitiesV1 {
        streaming: true,
        tools: true,
        parallel_tool_calls: true,
        vision: true,
        reasoning: true,
        prompt_cache_usage: true,
        ..ProviderCapabilitiesV1::default()
    };
    [
        ("claude-fable-5", "Claude Fable 5", 1_000_000u64, 131_072u64),
        ("claude-opus-4-8", "Claude Opus 4.8", 1_000_000, 131_072),
        ("claude-sonnet-5", "Claude Sonnet 5", 1_000_000, 131_072),
        ("claude-sonnet-4-6", "Claude Sonnet 4.6", 1_000_000, 65_536),
        (
            "claude-haiku-4-5-20251001",
            "Claude Haiku 4.5",
            200_000,
            65_536,
        ),
    ]
    .into_iter()
    .map(
        |(id, display_name, max_input, max_output)| ModelDescriptorV1 {
            id: id.into(),
            aliases: if id == "claude-haiku-4-5-20251001" {
                vec!["claude-haiku-4-5".into()]
            } else {
                Vec::new()
            },
            max_input_tokens: Some(max_input),
            max_output_tokens: Some(max_output),
            default_temperature: Some(1.0),
            capabilities: caps.clone(),
            endpoint_url: None,
            extensions: BTreeMap::from([("display_name".into(), serde_json::json!(display_name))]),
        },
    )
    .collect()
}

/// Curated Gemini model descriptors — catalog DATA (TD-0004). Sourced from Google's Gemini API
/// Models reference (ai.google.dev), current as of 2026-07. No pricing.
#[must_use]
pub fn gemini_models() -> Vec<ModelDescriptorV1> {
    let caps = ProviderCapabilitiesV1 {
        streaming: true,
        tools: true,
        parallel_tool_calls: true,
        vision: true,
        audio_input: true,
        file_input: true,
        structured_output: true,
        reasoning: true,
        prompt_cache_usage: true,
    };
    [
        ("gemini-3-pro", "Gemini 3 Pro", 1_048_576u64, 65_536u64),
        ("gemini-3-flash", "Gemini 3 Flash", 1_048_576, 65_536),
    ]
    .into_iter()
    .map(
        |(id, display_name, max_input, max_output)| ModelDescriptorV1 {
            id: id.into(),
            aliases: Vec::new(),
            max_input_tokens: Some(max_input),
            max_output_tokens: Some(max_output),
            default_temperature: Some(1.0),
            capabilities: caps.clone(),
            endpoint_url: None,
            extensions: BTreeMap::from([("display_name".into(), serde_json::json!(display_name))]),
        },
    )
    .collect()
}

/// Curated OpenAI model descriptors — catalog DATA (TD-0004). Sourced from OpenAI's API Models
/// reference (developers.openai.com), current as of 2026-07. No pricing.
#[must_use]
pub fn openai_models() -> Vec<ModelDescriptorV1> {
    let caps = ProviderCapabilitiesV1 {
        streaming: true,
        tools: true,
        parallel_tool_calls: true,
        vision: true,
        reasoning: true,
        prompt_cache_usage: true,
        structured_output: true,
        ..ProviderCapabilitiesV1::default()
    };
    [
        ("gpt-5", "GPT-5", 400_000u64, 128_000u64),
        ("gpt-5-chat-latest", "GPT-5 Chat", 128_000, 16_384),
    ]
    .into_iter()
    .map(
        |(id, display_name, max_input, max_output)| ModelDescriptorV1 {
            id: id.into(),
            aliases: Vec::new(),
            max_input_tokens: Some(max_input),
            max_output_tokens: Some(max_output),
            default_temperature: Some(1.0),
            capabilities: caps.clone(),
            endpoint_url: None,
            extensions: BTreeMap::from([("display_name".into(), serde_json::json!(display_name))]),
        },
    )
    .collect()
}

/// Curated model descriptors for a native (non-OpenAI-compat) provider slug.
/// Admit new providers here; an unknown slug yields an empty list (no invented facts).
#[must_use]
pub fn native_models(slug: &str) -> Vec<ModelDescriptorV1> {
    match slug {
        "anthropic" => anthropic_models(),
        "gemini" => gemini_models(),
        _ => Vec::new(),
    }
}

/// Shared builder for a curated OpenAI-compat model entry. `max_output_tokens` is
/// `None` where the vendor does not document a stable cap — no invented facts.
fn compat_entry(
    id: &str,
    display_name: &str,
    max_input: u64,
    max_output: Option<u64>,
    capabilities: ProviderCapabilitiesV1,
) -> ModelDescriptorV1 {
    ModelDescriptorV1 {
        id: id.into(),
        aliases: Vec::new(),
        max_input_tokens: Some(max_input),
        max_output_tokens: max_output,
        default_temperature: Some(1.0),
        capabilities,
        endpoint_url: None,
        extensions: BTreeMap::from([("display_name".into(), serde_json::json!(display_name))]),
    }
}

/// Baseline capabilities every seeded compat lineup shares (chat + streaming + tools).
fn compat_base_caps() -> ProviderCapabilitiesV1 {
    ProviderCapabilitiesV1 {
        streaming: true,
        tools: true,
        parallel_tool_calls: true,
        ..ProviderCapabilitiesV1::default()
    }
}

/// Curated model descriptors for an OpenAI-compatible provider slug (TD-0004).
///
/// Seeding policy: **first-party model vendors** (openai, moonshot, xai, deepseek,
/// mistral, zai, qwen) plus the stable flagship rows of hosting providers
/// (groq, cerebras) carry curated lineups; **aggregators** (together, fireworks,
/// openrouter) deliberately stay empty — their hosting catalogs are dynamic, so
/// consumers use live `GET /models` discovery there. Facts current as of 2026-07;
/// no pricing (measure-vs-price line held). An unknown slug yields an empty list.
fn compat_models(slug: &str) -> Vec<ModelDescriptorV1> {
    let base = compat_base_caps();
    let reasoning = ProviderCapabilitiesV1 {
        reasoning: true,
        ..compat_base_caps()
    };
    match slug {
        "moonshot" => vec![ModelDescriptorV1 {
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
        }],
        "openai" => openai_models(),
        "xai" => vec![
            compat_entry(
                "grok-4-1-fast",
                "Grok 4.1 Fast",
                2_000_000,
                None,
                ProviderCapabilitiesV1 {
                    vision: true,
                    ..reasoning.clone()
                },
            ),
            compat_entry(
                "grok-4",
                "Grok 4",
                256_000,
                None,
                ProviderCapabilitiesV1 {
                    vision: true,
                    ..reasoning.clone()
                },
            ),
        ],
        "deepseek" => vec![
            compat_entry(
                "deepseek-chat",
                "DeepSeek Chat",
                131_072,
                Some(8_192),
                ProviderCapabilitiesV1 {
                    prompt_cache_usage: true,
                    ..base.clone()
                },
            ),
            compat_entry(
                "deepseek-reasoner",
                "DeepSeek Reasoner",
                131_072,
                Some(65_536),
                ProviderCapabilitiesV1 {
                    // Function calling is not supported on the reasoner endpoint.
                    tools: false,
                    parallel_tool_calls: false,
                    prompt_cache_usage: true,
                    ..reasoning.clone()
                },
            ),
        ],
        "mistral" => vec![
            compat_entry(
                "mistral-large-latest",
                "Mistral Large",
                131_072,
                None,
                ProviderCapabilitiesV1 {
                    structured_output: true,
                    ..base.clone()
                },
            ),
            compat_entry("codestral-latest", "Codestral", 262_144, None, base.clone()),
        ],
        "zai" => vec![
            compat_entry(
                "glm-4.6",
                "GLM-4.6",
                204_800,
                Some(131_072),
                reasoning.clone(),
            ),
            compat_entry("glm-4.5-air", "GLM-4.5 Air", 131_072, None, reasoning),
        ],
        "qwen" => vec![
            compat_entry(
                "qwen3-max",
                "Qwen3 Max",
                262_144,
                Some(65_536),
                base.clone(),
            ),
            compat_entry(
                "qwen3-coder-plus",
                "Qwen3 Coder Plus",
                1_048_576,
                Some(65_536),
                base.clone(),
            ),
        ],
        "groq" => vec![compat_entry(
            "llama-3.3-70b-versatile",
            "Llama 3.3 70B Versatile",
            131_072,
            Some(32_768),
            base.clone(),
        )],
        "cerebras" => vec![compat_entry(
            "llama-3.3-70b",
            "Llama 3.3 70B",
            131_072,
            None,
            base,
        )],
        _ => Vec::new(),
    }
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
        models: native_models(slug),
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
        // Aggregators carry no curated models (dynamic hosting catalogs — no invented facts).
        assert!(openai_compat_descriptor("together")
            .unwrap()
            .models
            .is_empty());
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

    #[test]
    fn anthropic_catalog_admits_current_model_data() {
        let descriptor = provider_descriptor("anthropic").unwrap();
        let ids: Vec<&str> = descriptor.models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"claude-fable-5"));
        assert!(ids.contains(&"claude-opus-4-8"));
        assert!(ids.contains(&"claude-sonnet-5"));
        assert!(ids.contains(&"claude-haiku-4-5-20251001"));
        // Facts only — context window + max output; no pricing (measure-vs-price line held).
        let fable = descriptor
            .models
            .iter()
            .find(|m| m.id == "claude-fable-5")
            .unwrap();
        assert_eq!(fable.max_input_tokens, Some(1_000_000));
        assert_eq!(fable.max_output_tokens, Some(131_072));
        assert!(fable.extensions.contains_key("display_name"));
        // Unknown providers resolve to an empty catalog — Sandhi never invents model facts.
        assert!(native_models("unknown-provider").is_empty());
    }

    #[test]
    fn gemini_and_openai_catalogs_admit_current_model_data() {
        let gemini = provider_descriptor("google").unwrap();
        let g_ids: Vec<&str> = gemini.models.iter().map(|m| m.id.as_str()).collect();
        assert!(g_ids.contains(&"gemini-3-pro"));
        assert!(g_ids.contains(&"gemini-3-flash"));
        let g_pro = gemini
            .models
            .iter()
            .find(|m| m.id == "gemini-3-pro")
            .unwrap();
        assert_eq!(g_pro.max_input_tokens, Some(1_048_576));
        assert_eq!(g_pro.max_output_tokens, Some(65_536));

        let openai = provider_descriptor("openai").unwrap();
        let o_ids: Vec<&str> = openai.models.iter().map(|m| m.id.as_str()).collect();
        assert!(o_ids.contains(&"gpt-5"));
        assert!(o_ids.contains(&"gpt-5-chat-latest"));
        let gpt5 = openai.models.iter().find(|m| m.id == "gpt-5").unwrap();
        assert_eq!(gpt5.max_input_tokens, Some(400_000));
        assert_eq!(gpt5.max_output_tokens, Some(128_000));

        // Aggregators stay deliberately empty (dynamic hosting catalogs — live discovery).
        assert!(provider_descriptor("openrouter").unwrap().models.is_empty());
        assert!(provider_descriptor("together").unwrap().models.is_empty());
        assert!(provider_descriptor("fireworks").unwrap().models.is_empty());
    }

    #[test]
    fn compat_vendor_catalogs_admit_curated_lineups() {
        // First-party vendors (and stable hosting flagships) carry curated model facts.
        for (slug, expected_id) in [
            ("xai", "grok-4-1-fast"),
            ("deepseek", "deepseek-chat"),
            ("mistral", "mistral-large-latest"),
            ("zai", "glm-4.6"),
            ("qwen", "qwen3-max"),
            ("groq", "llama-3.3-70b-versatile"),
            ("cerebras", "llama-3.3-70b"),
        ] {
            let descriptor = provider_descriptor(slug).unwrap();
            let ids: Vec<&str> = descriptor.models.iter().map(|m| m.id.as_str()).collect();
            assert!(ids.contains(&expected_id), "{slug} missing {expected_id}");
            for model in &descriptor.models {
                assert!(
                    model.extensions.contains_key("display_name"),
                    "{slug}/{} lacks display_name",
                    model.id
                );
                assert!(model.max_input_tokens.is_some());
            }
        }
        // Alias resolution reaches the same lineup.
        let grok = provider_descriptor("grok").unwrap();
        assert!(grok.models.iter().any(|m| m.id == "grok-4"));
        // The reasoner endpoint does not admit function calling (fact, not policy).
        let deepseek = provider_descriptor("deepseek").unwrap();
        let reasoner = deepseek
            .models
            .iter()
            .find(|m| m.id == "deepseek-reasoner")
            .unwrap();
        assert!(!reasoner.capabilities.tools);
        assert!(reasoner.capabilities.reasoning);
    }
}
