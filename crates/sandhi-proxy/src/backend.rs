//! Explicit endpoint binding, separate from provider labels and credential references.
//!
//! This is the factory seam for TD-0034 M2a. Registry persistence, routing and
//! operator configuration use separate milestones; this type grants no authority.
use sandhi_core::EndpointFamilyV1;
use sandhi_providers::ProviderFamily;
pub use sandhi_store::backends::{BackendDefinition, BackendEndpoint, BackendProfile};
use sandhi_store::CredentialScheme;

mod registry;
pub(crate) use registry::{list, preflight_config, publish_credential, register, revoke};
pub use registry::{rehydrate_backends, RegisterBackendRequest};

pub(crate) struct ResolvedEndpoint {
    pub family: ProviderFamily,
    pub profile: BackendProfile,
}

/// Resolve legacy defaults once, then validate before any transport or vault mutation.
/// Standard endpoints accept API keys/static bearer credentials; OAuth requires
/// an implemented lease profile, not an accidental API-key-header fallback.
pub(crate) fn resolve(
    provider: &str,
    scheme: CredentialScheme,
    endpoint: Option<BackendEndpoint>,
) -> Option<ResolvedEndpoint> {
    let endpoint = endpoint.unwrap_or_else(|| {
        if provider == "openai" && scheme == CredentialScheme::Oauth {
            return BackendEndpoint {
                endpoint_family: EndpointFamilyV1::OpenaiResponses,
                endpoint_profile: BackendProfile::ChatGptCodex,
            };
        }
        BackendEndpoint::standard(match ProviderFamily::for_slug(provider) {
            ProviderFamily::OpenAiCompat => EndpointFamilyV1::OpenaiChatCompletions,
            ProviderFamily::OpenAiResponses => EndpointFamilyV1::OpenaiResponses,
            ProviderFamily::Anthropic => EndpointFamilyV1::AnthropicMessages,
            ProviderFamily::Gemini => EndpointFamilyV1::GeminiGenerateContent,
            ProviderFamily::Cohere => EndpointFamilyV1::CohereChat,
            ProviderFamily::Ollama => EndpointFamilyV1::OllamaChat,
        })
    });
    let family = match endpoint.endpoint_family {
        EndpointFamilyV1::OpenaiChatCompletions => ProviderFamily::OpenAiCompat,
        EndpointFamilyV1::OpenaiResponses => ProviderFamily::OpenAiResponses,
        EndpointFamilyV1::AnthropicMessages => ProviderFamily::Anthropic,
        EndpointFamilyV1::GeminiGenerateContent => ProviderFamily::Gemini,
        EndpointFamilyV1::CohereChat => ProviderFamily::Cohere,
        EndpointFamilyV1::OllamaChat => ProviderFamily::Ollama,
        EndpointFamilyV1::BedrockConverse | EndpointFamilyV1::HostCallback => return None,
    };
    match endpoint.endpoint_profile {
        BackendProfile::ChatGptCodex
            if family == ProviderFamily::OpenAiResponses && scheme == CredentialScheme::Oauth => {}
        BackendProfile::Standard if scheme != CredentialScheme::Oauth => {}
        BackendProfile::AnthropicImplicitCache
            if family == ProviderFamily::Anthropic && scheme != CredentialScheme::Oauth => {}
        BackendProfile::OpenaiTerminalChunkUsage
            if family == ProviderFamily::OpenAiCompat && scheme != CredentialScheme::Oauth => {}
        _ => return None,
    }
    Some(ResolvedEndpoint {
        family,
        profile: endpoint.endpoint_profile,
    })
}
