//! Codec-owned ordered input plus a consistency fence over its portable projection.
//!
//! Input bytes remain in the existing native extensions bucket; this adds no second
//! prompt copy. A digest is not provenance authentication: HTTP decoders must construct
//! this field themselves and never import it from client-controlled extras.
use crate::{ChatRequestV1, EndpointFamilyV1};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NativeInputFeatureV1 {
    OpaqueState,
    ResourceReference,
    UnsupportedContent,
    NativeControl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeInputV1 {
    pub endpoint_family: EndpointFamilyV1,
    /// SHA-256 of native input fields, portable messages/tools/choice and feature flags.
    /// Excludes resolved model, output bounds and authenticated attribution.
    pub projection_sha256: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nonportable_features: Vec<NativeInputFeatureV1>,
}

fn fields(family: EndpointFamilyV1) -> Result<(&'static str, &'static [&'static str]), String> {
    use EndpointFamilyV1::*;
    match family {
        OpenaiChatCompletions => Ok((
            "openai",
            &[
                "messages",
                "tools",
                "tool_choice",
                "functions",
                "function_call",
            ],
        )),
        OpenaiResponses => Ok((
            "openai_responses",
            &[
                "input",
                "instructions",
                "tools",
                "tool_choice",
                "previous_response_id",
                "conversation",
            ],
        )),
        AnthropicMessages => Ok(("anthropic", &["system", "messages", "tools", "tool_choice"])),
        GeminiGenerateContent => Ok((
            "gemini",
            &[
                "systemInstruction",
                "contents",
                "tools",
                "toolConfig",
                "cachedContent",
            ],
        )),
        _ => Err("native input capture is not implemented for this endpoint family".into()),
    }
}

struct DigestWriter(Sha256);
impl io::Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ChatRequestV1 {
    fn native_projection_sha256(&self, context: &NativeInputV1) -> Result<String, String> {
        let (bucket, input_fields) = fields(context.endpoint_family)?;
        let body = self
            .extensions
            .get(bucket)
            .and_then(Value::as_object)
            .ok_or("native input requires its original family body")?;
        let input: BTreeMap<_, _> = input_fields
            .iter()
            .filter_map(|key| body.get(*key).map(|value| (*key, value)))
            .collect();
        let mut writer = DigestWriter(Sha256::new());
        // Stream serialization into the digest; no extra serialized prompt buffer.
        serde_json::to_writer(
            &mut writer,
            &(
                context.endpoint_family,
                &input,
                &self.messages,
                &self.tools,
                &self.tool_choice,
                &context.nonportable_features,
            ),
        )
        .map_err(|_| "native input projection cannot be serialized")?;
        Ok(format!("{:x}", writer.0.finalize()))
    }

    /// Called by a trusted ingress codec after constructing the portable projection.
    /// Direct typed hosts may use this too; it confers no route or credential authority.
    pub fn capture_native_input(
        &mut self,
        endpoint_family: EndpointFamilyV1,
        nonportable_features: Vec<NativeInputFeatureV1>,
    ) -> Result<(), String> {
        // At most four distinct, static feature classes; never arbitrary native diagnostics.
        let mut unique = Vec::new();
        for feature in nonportable_features {
            if !unique.contains(&feature) {
                unique.push(feature);
            }
        }
        let mut context = NativeInputV1 {
            endpoint_family,
            projection_sha256: String::new(),
            nonportable_features: unique,
        };
        context.projection_sha256 = self.native_projection_sha256(&context)?;
        self.native_input = Some(context);
        Ok(())
    }

    /// A compatible codec may reuse exact ordered input only while its projection
    /// still matches. Nonportable content/state/control never silently crosses families.
    pub fn native_input_for(
        &self,
        target: EndpointFamilyV1,
    ) -> Result<Option<&Map<String, Value>>, String> {
        let Some(context) = &self.native_input else {
            return Ok(None);
        };
        if context.nonportable_features.len() > 4
            || context.projection_sha256.len() != 64
            || context.projection_sha256 != self.native_projection_sha256(context)?
        {
            return Err("native input no longer matches its portable projection; decode the updated input again".into());
        }
        if target != context.endpoint_family {
            if !context.nonportable_features.is_empty() {
                return Err("native request contains content, state or controls that cannot be translated to this endpoint family".into());
            }
            return Ok(None);
        }
        let (bucket, _) = fields(target)?;
        Ok(self.extensions.get(bucket).and_then(Value::as_object))
    }

    /// Restore only input fields, preserving encode-authoritative model/limits.
    pub fn restore_native_input(
        &self,
        target: EndpointFamilyV1,
        body: &mut Map<String, Value>,
    ) -> Result<bool, String> {
        let Some(native) = self.native_input_for(target)? else {
            return Ok(false);
        };
        for key in fields(target)?.1 {
            match native.get(*key) {
                Some(value) => {
                    body.insert((*key).into(), value.clone());
                }
                None => {
                    body.remove(*key);
                }
            }
        }
        Ok(true)
    }
}
