#![allow(clippy::redundant_closure_call)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::match_single_binding)]
#![allow(clippy::clone_on_copy)]

#[doc = r" Error types."]
pub mod error {
    #[doc = r" Error from a `TryFrom` or `FromStr` implementation."]
    pub struct ConversionError(::std::borrow::Cow<'static, str>);
    impl ::std::error::Error for ConversionError {}
    impl ::std::fmt::Display for ConversionError {
        fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> Result<(), ::std::fmt::Error> {
            ::std::fmt::Display::fmt(&self.0, f)
        }
    }
    impl ::std::fmt::Debug for ConversionError {
        fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> Result<(), ::std::fmt::Error> {
            ::std::fmt::Debug::fmt(&self.0, f)
        }
    }
    impl From<&'static str> for ConversionError {
        fn from(value: &'static str) -> Self {
            Self(value.into())
        }
    }
    impl From<String> for ConversionError {
        fn from(value: String) -> Self {
            Self(value.into())
        }
    }
}
#[doc = "Token usage for an Anthropic Messages response, including the prompt-cache split."]
#[doc = r""]
#[doc = r" <details><summary>JSON schema</summary>"]
#[doc = r""]
#[doc = r" ```json"]
#[doc = "{"]
#[doc = "  \"title\": \"AnthropicMessageUsage\","]
#[doc = "  \"description\": \"Token usage for an Anthropic Messages response, including the prompt-cache split.\","]
#[doc = "  \"type\": \"object\","]
#[doc = "  \"properties\": {"]
#[doc = "    \"cache_creation_input_tokens\": {"]
#[doc = "      \"description\": \"Prompt-cache write tokens.\","]
#[doc = "      \"type\": \"integer\""]
#[doc = "    },"]
#[doc = "    \"cache_read_input_tokens\": {"]
#[doc = "      \"description\": \"Prompt-cache read tokens.\","]
#[doc = "      \"type\": \"integer\""]
#[doc = "    },"]
#[doc = "    \"input_tokens\": {"]
#[doc = "      \"description\": \"Fresh (non-cached) input tokens.\","]
#[doc = "      \"type\": \"integer\""]
#[doc = "    },"]
#[doc = "    \"output_tokens\": {"]
#[doc = "      \"description\": \"Output tokens.\","]
#[doc = "      \"type\": \"integer\""]
#[doc = "    }"]
#[doc = "  },"]
#[doc = "  \"$comment\": \"Provenance: Anthropic Messages API `usage` object, hand-authored from the public API documentation (Anthropic publishes no official OpenAPI spec — ADR-0003 context). Byte-pinned SHIPPED-codegen source for TD-0001 W3 (ADR-0003 §2/§4). Regenerate crates/sandhi-core/src/generated/anthropic_usage.rs via scripts/gen-provider-models.sh — never hand-edit the generated file. All fields optional so the parser stays lenient (missing → 0), matching the prior u64_at behavior.\""]
#[doc = "}"]
#[doc = r" ```"]
#[doc = r" </details>"]
#[derive(:: serde :: Deserialize, :: serde :: Serialize, Clone, Debug)]
pub struct AnthropicMessageUsage {
    #[doc = "Prompt-cache write tokens."]
    #[serde(default, skip_serializing_if = "::std::option::Option::is_none")]
    pub cache_creation_input_tokens: ::std::option::Option<i64>,
    #[doc = "Prompt-cache read tokens."]
    #[serde(default, skip_serializing_if = "::std::option::Option::is_none")]
    pub cache_read_input_tokens: ::std::option::Option<i64>,
    #[doc = "Fresh (non-cached) input tokens."]
    #[serde(default, skip_serializing_if = "::std::option::Option::is_none")]
    pub input_tokens: ::std::option::Option<i64>,
    #[doc = "Output tokens."]
    #[serde(default, skip_serializing_if = "::std::option::Option::is_none")]
    pub output_tokens: ::std::option::Option<i64>,
}
impl ::std::convert::From<&AnthropicMessageUsage> for AnthropicMessageUsage {
    fn from(value: &AnthropicMessageUsage) -> Self {
        value.clone()
    }
}
impl ::std::default::Default for AnthropicMessageUsage {
    fn default() -> Self {
        Self {
            cache_creation_input_tokens: Default::default(),
            cache_read_input_tokens: Default::default(),
            input_tokens: Default::default(),
            output_tokens: Default::default(),
        }
    }
}
impl AnthropicMessageUsage {
    pub fn builder() -> builder::AnthropicMessageUsage {
        Default::default()
    }
}
#[doc = r" Types for composing complex structures."]
pub mod builder {
    #[derive(Clone, Debug)]
    pub struct AnthropicMessageUsage {
        cache_creation_input_tokens:
            ::std::result::Result<::std::option::Option<i64>, ::std::string::String>,
        cache_read_input_tokens:
            ::std::result::Result<::std::option::Option<i64>, ::std::string::String>,
        input_tokens: ::std::result::Result<::std::option::Option<i64>, ::std::string::String>,
        output_tokens: ::std::result::Result<::std::option::Option<i64>, ::std::string::String>,
    }
    impl ::std::default::Default for AnthropicMessageUsage {
        fn default() -> Self {
            Self {
                cache_creation_input_tokens: Ok(Default::default()),
                cache_read_input_tokens: Ok(Default::default()),
                input_tokens: Ok(Default::default()),
                output_tokens: Ok(Default::default()),
            }
        }
    }
    impl AnthropicMessageUsage {
        pub fn cache_creation_input_tokens<T>(mut self, value: T) -> Self
        where
            T: ::std::convert::TryInto<::std::option::Option<i64>>,
            T::Error: ::std::fmt::Display,
        {
            self.cache_creation_input_tokens = value.try_into().map_err(|e| {
                format!(
                    "error converting supplied value for cache_creation_input_tokens: {}",
                    e
                )
            });
            self
        }
        pub fn cache_read_input_tokens<T>(mut self, value: T) -> Self
        where
            T: ::std::convert::TryInto<::std::option::Option<i64>>,
            T::Error: ::std::fmt::Display,
        {
            self.cache_read_input_tokens = value.try_into().map_err(|e| {
                format!(
                    "error converting supplied value for cache_read_input_tokens: {}",
                    e
                )
            });
            self
        }
        pub fn input_tokens<T>(mut self, value: T) -> Self
        where
            T: ::std::convert::TryInto<::std::option::Option<i64>>,
            T::Error: ::std::fmt::Display,
        {
            self.input_tokens = value
                .try_into()
                .map_err(|e| format!("error converting supplied value for input_tokens: {}", e));
            self
        }
        pub fn output_tokens<T>(mut self, value: T) -> Self
        where
            T: ::std::convert::TryInto<::std::option::Option<i64>>,
            T::Error: ::std::fmt::Display,
        {
            self.output_tokens = value
                .try_into()
                .map_err(|e| format!("error converting supplied value for output_tokens: {}", e));
            self
        }
    }
    impl ::std::convert::TryFrom<AnthropicMessageUsage> for super::AnthropicMessageUsage {
        type Error = super::error::ConversionError;
        fn try_from(
            value: AnthropicMessageUsage,
        ) -> ::std::result::Result<Self, super::error::ConversionError> {
            Ok(Self {
                cache_creation_input_tokens: value.cache_creation_input_tokens?,
                cache_read_input_tokens: value.cache_read_input_tokens?,
                input_tokens: value.input_tokens?,
                output_tokens: value.output_tokens?,
            })
        }
    }
    impl ::std::convert::From<super::AnthropicMessageUsage> for AnthropicMessageUsage {
        fn from(value: super::AnthropicMessageUsage) -> Self {
            Self {
                cache_creation_input_tokens: Ok(value.cache_creation_input_tokens),
                cache_read_input_tokens: Ok(value.cache_read_input_tokens),
                input_tokens: Ok(value.input_tokens),
                output_tokens: Ok(value.output_tokens),
            }
        }
    }
}
