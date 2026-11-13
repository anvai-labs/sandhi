//! Shared binding factory. Endpoint conventions never come from credential bytes.
use crate::{
    AnthropicAuthScheme, GeminiAuthScheme, ProviderError, ProviderFamily, ProviderHandle,
    ProviderRuntime,
};
use sandhi_core::{usage::AnthropicUsageProfile, EndpointProfileV1 as Profile};

/// Constructor inputs shared by language bindings. Intentionally not Debug:
/// api_key and headers may contain credentials. No source policy enters a request body.
pub struct ProviderOptions {
    pub provider: String,
    pub model: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub headers: http::HeaderMap,
    pub max_retries: Option<u32>,
    pub timeout_secs: Option<f64>,
    pub stream_setup_timeout_secs: Option<f64>,
    pub stream_idle_timeout_secs: Option<f64>,
    pub auth_scheme: Option<String>,
    pub protocol: Option<String>,
    pub endpoint_profile: Option<String>,
    pub peer_role: Option<String>,
}

/// Provider identity for the binding, separate from the protocol implementation.
pub struct ConfiguredProvider {
    pub provider: String,
    pub handle: ProviderHandle,
}

fn invalid(message: &str) -> ProviderError {
    ProviderError::InvalidRequest(message.into())
}

fn default_family(provider: &str) -> ProviderFamily {
    match provider {
        "anthropic" | "claude" => ProviderFamily::Anthropic,
        "gemini" | "google" => ProviderFamily::Gemini,
        _ => ProviderFamily::for_slug(provider),
    }
}

fn protocol(
    provider: &str,
    value: Option<&str>,
) -> Result<(ProviderFamily, Profile), ProviderError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok((default_family(provider), Profile::Standard));
    };
    if value.len() > 64 {
        return Err(invalid("unsupported protocol"));
    }
    let family = match value.trim().to_ascii_lowercase().as_str() {
        "chat_completions" | "openai_chat_completions" => ProviderFamily::OpenAiCompat,
        "responses" | "openai_responses" => ProviderFamily::OpenAiResponses,
        "chatgpt_responses" | "codex_responses" => {
            return Ok((ProviderFamily::OpenAiResponses, Profile::ChatGptCodex))
        }
        "anthropic" | "anthropic_messages" | "messages" => ProviderFamily::Anthropic,
        "gemini" | "gemini_generate_content" | "generate_content" => ProviderFamily::Gemini,
        "cohere" | "cohere_chat" => ProviderFamily::Cohere,
        "ollama" | "ollama_chat" => ProviderFamily::Ollama,
        _ => return Err(invalid("unsupported protocol")),
    };
    Ok((family, Profile::Standard))
}

impl ProviderRuntime {
    /// Validate protocol, profile and authentication before constructing a transport.
    /// This reuses existing family factories; it adds no dispatcher, fallback or ledger.
    pub fn configured_provider(
        &self,
        options: ProviderOptions,
    ) -> Result<ConfiguredProvider, ProviderError> {
        for seconds in [
            options.timeout_secs,
            options.stream_setup_timeout_secs,
            options.stream_idle_timeout_secs,
        ]
        .into_iter()
        .flatten()
        {
            if !seconds.is_finite()
                || std::time::Duration::try_from_secs_f64(seconds.max(0.001))
                    .ok()
                    .and_then(|duration| std::time::Instant::now().checked_add(duration))
                    .is_none()
            {
                return Err(invalid(
                    "timeout bounds must be finite representable durations",
                ));
            }
        }
        let mut slug = options.provider.trim().to_ascii_lowercase();
        let (family, default_profile) = protocol(&slug, options.protocol.as_deref())?;
        let role = options
            .peer_role
            .as_deref()
            .unwrap_or("provider")
            .parse::<sandhi_core::PeerRoleV1>()
            .map_err(invalid)?;
        if role == sandhi_core::PeerRoleV1::SandhiGatewayV1 {
            if options
                .protocol
                .as_deref()
                .is_none_or(|protocol| protocol.trim().is_empty())
                || default_profile != Profile::Standard
                || options.endpoint_profile.is_some()
                || options
                    .base_url
                    .as_deref()
                    .is_none_or(|base| !crate::peer::valid_gateway_base(base))
                || options.api_key.is_empty()
                || options.max_retries.unwrap_or(0) != 0
                || options
                    .auth_scheme
                    .as_deref()
                    .is_some_and(|auth| auth != "bearer")
                || options.headers.contains_key("chatgpt-account-id")
                || sandhi_core::UsageConventionV1::standard(family.endpoint_family()).is_err()
            {
                return Err(invalid("gateway peer requires explicit client protocol and base_url, bearer auth, zero retries and no backend endpoint profile or account header"));
            }
            let mut config = crate::ProviderTransportConfig::new(
                family,
                slug.clone(),
                options.base_url.expect("validated base"),
                options.api_key,
            );
            config.headers = options.headers;
            config.max_retries = Some(0);
            config.timeout_secs = options.timeout_secs;
            config.stream_setup_timeout_secs = options.stream_setup_timeout_secs;
            config.stream_idle_timeout_secs = options.stream_idle_timeout_secs;
            return Ok(ConfiguredProvider {
                provider: slug.clone(),
                handle: self
                    .gateway_peer(config)
                    .with_provider_identity(slug.clone())?,
            });
        }
        let profile = options
            .endpoint_profile
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::parse::<Profile>)
            .transpose()
            .map_err(invalid)?
            .unwrap_or(default_profile);
        profile
            .validate_family(family.endpoint_family())
            .map_err(invalid)?;
        if default_profile == Profile::ChatGptCodex && profile != default_profile {
            return Err(invalid(
                "chatgpt_responses requires the chatgpt_codex endpoint profile",
            ));
        }
        let auth = options
            .auth_scheme
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        if auth.is_some_and(|value| value.len() > 32) {
            return Err(invalid("unsupported auth_scheme"));
        }
        let normalized_auth = auth.map(|value| value.trim().to_ascii_lowercase());
        let bearer = match normalized_auth.as_deref() {
            None => false,
            Some("bearer") => true,
            Some("api_key")
                if matches!(family, ProviderFamily::Anthropic | ProviderFamily::Gemini) =>
            {
                false
            }
            _ => return Err(invalid(
                "auth_scheme must match the selected protocol; api_key is only valid for the Anthropic/Gemini protocols, and bearer is a no-op for other protocols",
            )),
        };
        let base_url = match options.base_url {
            Some(base) => base,
            None if matches!(family, ProviderFamily::OpenAiCompat | ProviderFamily::OpenAiResponses) => {
                let spec = crate::resolve_openai_compat_provider(&slug)
                    .ok_or_else(|| invalid("unknown catalog provider; base_url is required"))?;
                if family == ProviderFamily::OpenAiCompat {
                    slug = spec.slug.into();
                }
                spec.base_url_for_model(&options.model).into()
            }
            None if default_family(&slug) == family => family.default_base_url().into(),
            None => return Err(invalid(
                "base_url is required when the explicit protocol differs from the provider's default",
            )),
        };
        let h = options.headers;
        let retry = options.max_retries;
        let timeout = options.timeout_secs;
        let setup = options.stream_setup_timeout_secs;
        let idle = options.stream_idle_timeout_secs;
        let provider = match slug.as_str() {
            "claude" => "anthropic".into(),
            "google" => "gemini".into(),
            _ => slug.clone(),
        };
        let handle = match family {
            ProviderFamily::OpenAiCompat => self.openai_compat_with_stream_usage_profile(
                slug,
                base_url,
                options.api_key,
                h,
                retry,
                timeout,
                setup,
                idle,
                profile.openai_stream_usage_profile(),
            ),
            ProviderFamily::OpenAiResponses if profile == Profile::ChatGptCodex => self
                .chatgpt_responses(
                    slug,
                    base_url,
                    options.api_key,
                    h,
                    retry,
                    timeout,
                    setup,
                    idle,
                ),
            ProviderFamily::OpenAiResponses => self.openai_responses(
                slug,
                base_url,
                options.api_key,
                h,
                retry,
                timeout,
                setup,
                idle,
            ),
            ProviderFamily::Anthropic => self.anthropic_with_usage_profile(
                base_url,
                options.api_key,
                if bearer {
                    AnthropicAuthScheme::Bearer
                } else {
                    AnthropicAuthScheme::ApiKey
                },
                h,
                retry,
                timeout,
                setup,
                idle,
                if profile == Profile::AnthropicImplicitCache {
                    AnthropicUsageProfile::ImplicitCache
                } else {
                    AnthropicUsageProfile::SeparateCacheWrites
                },
            ),
            ProviderFamily::Gemini => self.gemini(
                base_url,
                options.api_key,
                if bearer {
                    GeminiAuthScheme::Bearer
                } else {
                    GeminiAuthScheme::ApiKey
                },
                h,
                retry,
                timeout,
                setup,
                idle,
            ),
            ProviderFamily::Cohere => {
                self.cohere(base_url, options.api_key, h, retry, timeout, setup, idle)
            }
            ProviderFamily::Ollama => {
                self.ollama(base_url, options.api_key, h, retry, timeout, setup, idle)
            }
        };
        let handle = handle.with_provider_identity(provider.clone())?;
        Ok(ConfiguredProvider { provider, handle })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(protocol: &str, profile: &str) -> ProviderOptions {
        ProviderOptions {
            provider: "fixture".into(),
            model: "fixture".into(),
            api_key: "synthetic".into(),
            base_url: Some("http://127.0.0.1:9".into()),
            headers: Default::default(),
            max_retries: Some(0),
            timeout_secs: None,
            stream_setup_timeout_secs: None,
            stream_idle_timeout_secs: None,
            auth_scheme: None,
            protocol: Some(protocol.into()),
            endpoint_profile: Some(profile.into()),
            peer_role: None,
        }
    }
    #[test]
    fn arbitrary_labels_and_explicit_protocols_do_not_select_each_other() {
        for (protocol, expected) in [
            ("chat_completions", ProviderFamily::OpenAiCompat),
            ("responses", ProviderFamily::OpenAiResponses),
            ("anthropic_messages", ProviderFamily::Anthropic),
            ("gemini_generate_content", ProviderFamily::Gemini),
            ("cohere_chat", ProviderFamily::Cohere),
            ("ollama_chat", ProviderFamily::Ollama),
        ] {
            let mut input = options(protocol, "standard");
            input.provider = "anthropic".into();
            let handle = ProviderRuntime::new().configured_provider(input).unwrap();
            assert_eq!(handle.handle.family(), expected);
            assert_eq!(handle.provider, "anthropic");
        }
    }
    #[test]
    fn incompatible_profiles_and_authentication_refuse_at_construction() {
        for (protocol, profile) in [
            ("responses", "openai_terminal_chunk_usage"),
            ("responses", "zai_final_choice_usage"),
            ("anthropic_messages", "deepseek_final_choice_usage"),
            ("anthropic_messages", "chatgpt_codex"),
            ("gemini_generate_content", "anthropic_implicit_cache"),
            ("chatgpt_responses", "standard"),
            ("chat_completions", "unsupported"),
        ] {
            assert!(matches!(
                ProviderRuntime::new().configured_provider(options(protocol, profile)),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
        for protocol in [
            "responses",
            "chat_completions",
            "chatgpt_responses",
            "cohere_chat",
            "ollama_chat",
        ] {
            let mut input = options(
                protocol,
                if protocol == "chatgpt_responses" {
                    "chatgpt_codex"
                } else {
                    "standard"
                },
            );
            input.auth_scheme = Some("api_key".into());
            assert!(matches!(
                ProviderRuntime::new().configured_provider(input),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }
    #[test]
    fn codex_remains_constrained_and_unknown_labels_need_an_explicit_destination() {
        for protocol in ["responses", "chatgpt_responses"] {
            let handle = ProviderRuntime::new()
                .configured_provider(options(protocol, "chatgpt_codex"))
                .unwrap();
            assert!(handle.handle.raw_forwarder().is_none());
        }
        for protocol in [
            "chat_completions",
            "responses",
            "anthropic_messages",
            "gemini_generate_content",
        ] {
            let mut input = options(protocol, "standard");
            input.base_url = None;
            assert!(matches!(
                ProviderRuntime::new().configured_provider(input),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn unrepresentable_timeouts_are_errors_instead_of_native_panics() {
        for bad in [f64::INFINITY, f64::NAN, f64::MAX] {
            let mut input = options("chat_completions", "standard");
            input.timeout_secs = Some(bad);
            assert!(matches!(
                ProviderRuntime::new().configured_provider(input),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }
    #[test]
    fn gateway_peer_requires_client_dialect_and_rejects_backend_authority() {
        for protocol in [
            "chat_completions",
            "anthropic_messages",
            "responses",
            "gemini_generate_content",
        ] {
            let mut input = options(protocol, "standard");
            input.endpoint_profile = None;
            input.peer_role = Some("sandhi_gateway_v1".into());
            let configured = ProviderRuntime::new().configured_provider(input).unwrap();
            assert!(configured.handle.raw_forwarder().is_none());
            assert!(!configured.handle.supports_owned_buffered());
            assert!(!configured.handle.supports_owned_streaming());
            let request = serde_json::from_value(serde_json::json!({"model":"fixture","messages":[{"role":"user","content":"hello"}]})).unwrap();
            assert!(configured.handle.compile_cache_plan(&request).is_err());
            assert!(configured
                .handle
                .apply_cache_plan(&request, &mut serde_json::json!({}))
                .is_err());
        }
        for bad in [
            "profile", "codex", "retry", "auth", "protocol", "origin", "account", "key",
        ] {
            let mut input = options("chat_completions", "standard");
            input.endpoint_profile = None;
            input.peer_role = Some("sandhi_gateway_v1".into());
            match bad {
                "profile" => input.endpoint_profile = Some("standard".into()),
                "codex" => input.protocol = Some("chatgpt_responses".into()),
                "retry" => input.max_retries = Some(1),
                "auth" => input.auth_scheme = Some("api_key".into()),
                "protocol" => input.protocol = None,
                "origin" => input.base_url = Some("http://public.example/v1".into()),
                "account" => {
                    input
                        .headers
                        .insert("chatgpt-account-id", "synthetic-account".parse().unwrap());
                }
                _ => input.api_key.clear(),
            }
            assert!(
                matches!(
                    ProviderRuntime::new().configured_provider(input),
                    Err(ProviderError::InvalidRequest(_))
                ),
                "{bad}"
            );
        }
    }
    #[test]
    fn source_conventions_survive_disabling_raw_forwarding_for_credential_fences() {
        for (protocol, profile, expected) in [
            (
                "chat_completions",
                "openai_terminal_chunk_usage",
                sandhi_core::UsageConventionV1::ChatTerminal,
            ),
            (
                "chat_completions",
                "zai_final_choice_usage",
                sandhi_core::UsageConventionV1::ChatZaiFinalChoice,
            ),
            (
                "chat_completions",
                "deepseek_final_choice_usage",
                sandhi_core::UsageConventionV1::ChatDeepseekFinalChoice,
            ),
            (
                "anthropic_messages",
                "anthropic_implicit_cache",
                sandhi_core::UsageConventionV1::MessagesImplicitCache,
            ),
        ] {
            let handle = ProviderRuntime::new()
                .configured_provider(options(protocol, profile))
                .unwrap()
                .handle;
            assert_eq!(handle.source_usage_convention().unwrap(), expected);
            let fenced = handle.with_credential_expiry(4_000_000_000);
            assert!(fenced.raw_forwarder().is_none());
            assert_eq!(fenced.source_usage_convention().unwrap(), expected);
        }
    }
}
