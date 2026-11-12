//! Cache intent and declared capabilities. Controls are not usage observations.
use crate::{ChatRequestV1, EndpointFamilyV1};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheModeV1 {
    PreserveNative,
    Auto,
    AutoStablePrefix,
    AutoConversationPrefix,
    SuppressGateway,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum CacheLifetimeV1 {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
    #[serde(rename = "30m")]
    ThirtyMinutes,
    #[serde(rename = "12h")]
    TwelveHours,
    #[serde(rename = "in_memory")]
    InMemory,
    #[serde(rename = "24h")]
    TwentyFourHours,
}
impl CacheLifetimeV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
            Self::ThirtyMinutes => "30m",
            Self::TwelveHours => "12h",
            Self::InMemory => "in_memory",
            Self::TwentyFourHours => "24h",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheIntentV1 {
    pub mode: CacheModeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<CacheLifetimeV1>,
}
impl CacheIntentV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.lifetime.is_some()
            && matches!(
                self.mode,
                CacheModeV1::PreserveNative | CacheModeV1::SuppressGateway
            )
        {
            return Err("preserve/suppress cache intent cannot select a lifetime");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheCapabilityProfileV1 {
    NativeOnly,
    ImplicitOnly,
    AnthropicBreakpoints,
    OpenaiLegacy,
    OpenaiExplicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheCapabilitiesV1 {
    pub profile: CacheCapabilityProfileV1,
    #[serde(default)]
    pub lifetimes: Vec<CacheLifetimeV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_breakpoints: Option<u16>,
}
impl CacheCapabilitiesV1 {
    pub fn validate(&self, family: EndpointFamilyV1) -> Result<(), &'static str> {
        use CacheCapabilityProfileV1 as P;
        use CacheLifetimeV1 as L;
        use EndpointFamilyV1 as F;
        let lifetimes: &[L] = match self.profile {
            P::NativeOnly | P::ImplicitOnly => &[],
            P::AnthropicBreakpoints if family == F::AnthropicMessages => {
                &[L::FiveMinutes, L::OneHour]
            }
            P::OpenaiLegacy if matches!(family, F::OpenaiChatCompletions | F::OpenaiResponses) => {
                &[L::InMemory, L::TwentyFourHours]
            }
            P::OpenaiExplicit if family == F::OpenaiResponses => {
                &[L::ThirtyMinutes, L::TwelveHours]
            }
            _ => return Err("cache capability profile is incompatible with endpoint family"),
        };
        if self
            .lifetimes
            .iter()
            .enumerate()
            .any(|(i, value)| !lifetimes.contains(value) || self.lifetimes[..i].contains(value))
        {
            return Err("invalid or duplicate cache lifetime for capability profile");
        }
        match (self.profile, self.max_breakpoints) {
            (P::AnthropicBreakpoints, Some(1..=4)) | (P::OpenaiExplicit, Some(1..=4)) => {}
            (P::NativeOnly | P::ImplicitOnly | P::OpenaiLegacy, None) => {}
            _ => return Err("cache capability requires a valid profile-specific breakpoint limit"),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheNamespaceV1 {
    #[default]
    ProviderDefault,
    ScopedKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackendCachePolicyV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_intent: Option<CacheIntentV1>,
    #[serde(default = "allow_override")]
    pub allow_request_override: bool,
    #[serde(default)]
    pub namespace: CacheNamespaceV1,
    pub models: BTreeMap<String, CacheCapabilitiesV1>,
}
fn allow_override() -> bool {
    true
}

/// Trusted execution data, deliberately not Deserialize/JsonSchema. HTTP/FFI
/// request JSON cannot import this field; the selected handle compiles it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CompiledCachePlanV1 {
    pub endpoint_family: EndpointFamilyV1,
    pub model: String,
    pub binding_sha256: String,
    pub input_sha256: String,
    pub intent: CacheIntentV1,
    pub capability: CacheCapabilityProfileV1,
    pub native_preserved: bool,
    pub scoped_key: Option<String>,
    pub mutations: Vec<CacheMutationV1>,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum CacheMutationV1 {
    Marker {
        pointer: String,
        field: String,
        value: Value,
    },
    AnthropicTextString {
        pointer: String,
        value: Value,
    },
    ResponsesTextString {
        pointer: String,
        value: Value,
    },
    TopLevel {
        field: String,
        value: Value,
    },
}

struct HashWriter(Sha256);
impl io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// Stream serialization into a delimiter-safe digest without another prompt buffer.
pub fn cache_digest(value: &impl Serialize) -> Result<String, String> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value).map_err(|_| "cache plan cannot be fingerprinted")?;
    Ok(format!("{:x}", writer.0.finalize()))
}
impl ChatRequestV1 {
    pub fn cache_input_sha256(&self) -> Result<String, String> {
        cache_digest(&(
            &self.model,
            &self.messages,
            &self.tools,
            &self.tool_choice,
            &self.metadata,
            &self.cache_intent,
            &self.native_input,
            &self.extensions,
        ))
    }
}
