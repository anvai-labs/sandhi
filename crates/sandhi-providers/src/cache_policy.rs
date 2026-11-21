//! Compile declared cache intent once, then apply the same bounded mutations on
//! typed and raw native bodies. No usage counters or inference replay live here.
use crate::{OpenAiResponsesProfile, ProviderError, ProviderFamily, ProviderHandle};
use sandhi_core::{
    cache_digest, chat::ThinkingEncodingV1, CacheCapabilitiesV1,
    CacheCapabilityProfileV1 as Profile, CacheIntentV1, CacheModeV1 as Mode,
    CacheMutationV1 as Mutation, CacheNamespaceV1 as Namespace, ChatRequestV1,
    CompiledCachePlanV1 as Plan, EndpointFamilyV1 as Family,
};
use serde_json::{json, Map, Value};

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::InvalidRequest(message.into())
}

fn openai_control_matches(value: &Value, thinking: &sandhi_core::chat::ThinkingRequestV1) -> bool {
    match value {
        Value::String(effort) => {
            if thinking.enabled {
                thinking.effort.as_deref() == Some(effort)
            } else {
                effort == "none"
            }
        }
        Value::Bool(enabled) => thinking.enabled == *enabled,
        Value::Number(budget) => {
            thinking.enabled
                && budget
                    .as_u64()
                    .is_some_and(|budget| thinking.budget_tokens == Some(budget))
        }
        Value::Object(native) => {
            let enabled = native.get("type").and_then(Value::as_str);
            let enabled = match enabled {
                Some("enabled") => true,
                Some("disabled") => false,
                _ => return false,
            };
            let budget = native.get("budget_tokens");
            enabled == thinking.enabled
                && match budget {
                    None | Some(Value::Null) => thinking.budget_tokens.is_none(),
                    Some(Value::Number(number)) => number
                        .as_u64()
                        .is_some_and(|budget| thinking.budget_tokens == Some(budget)),
                    _ => false,
                }
        }
        _ => false,
    }
}

fn openai_native_thinking_matches(body: &Map<String, Value>, request: &ChatRequestV1) -> bool {
    let Some(thinking) = &request.thinking else {
        return false;
    };
    let controls: Vec<&str> = [
        "reasoning_effort",
        "enable_thinking",
        "thinking_budget",
        "thinking",
    ]
    .iter()
    .filter(|key| {
        body.get(**key)
            .is_some_and(|value| !value.is_null() && openai_control_matches(value, thinking))
    })
    .copied()
    .collect();
    match controls.as_slice() {
        ["reasoning_effort"] | ["enable_thinking"] | ["thinking_budget"] | ["thinking"] => true,
        ["reasoning_effort", "thinking"] => {
            thinking.effort.is_some() && thinking.enabled && thinking.budget_tokens.is_none()
        }
        ["reasoning_effort", "thinking_budget"] => true,
        ["enable_thinking", "thinking_budget"] => {
            body.get("enable_thinking").and_then(Value::as_bool) == Some(true)
                && thinking.budget_tokens.is_some()
        }
        _ => false,
    }
}

fn responses_native_thinking_matches(body: &Map<String, Value>, request: &ChatRequestV1) -> bool {
    let Some(thinking) = &request.thinking else {
        return false;
    };
    let top_level = body.get("reasoning_effort");
    let nested = body
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("effort"));
    [top_level, nested]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_null())
        .all(|value| value.is_string() && openai_control_matches(value, thinking))
}

fn openai_gateway_thinking_encoding(request: &ChatRequestV1) -> ThinkingEncodingV1 {
    let native = request
        .native_input
        .as_ref()
        .filter(|source| source.endpoint_family == Family::OpenaiChatCompletions)
        .and_then(|_| request.extensions.get("openai"));
    let has = |key| {
        native
            .and_then(|body| body.get(key))
            .is_some_and(|value| !value.is_null())
    };
    if has("thinking") {
        ThinkingEncodingV1::ZaiOpenaiChat
    } else if has("enable_thinking") || has("thinking_budget") {
        ThinkingEncodingV1::QwenOpenaiChat
    } else {
        ThinkingEncodingV1::OpenaiChat
    }
}

fn openai_native_encoding_matches(
    body: &Map<String, Value>,
    request: &ChatRequestV1,
    encoding: ThinkingEncodingV1,
) -> bool {
    if !openai_native_thinking_matches(body, request) {
        return false;
    }
    let controls: Vec<&str> = [
        "reasoning_effort",
        "enable_thinking",
        "thinking_budget",
        "thinking",
    ]
    .iter()
    .filter(|key| body.get(**key).is_some_and(|value| !value.is_null()))
    .copied()
    .collect();
    match encoding {
        ThinkingEncodingV1::OpenaiChat => matches!(controls.as_slice(), ["reasoning_effort"]),
        ThinkingEncodingV1::QwenOpenaiChat => matches!(
            controls.as_slice(),
            ["reasoning_effort"]
                | ["enable_thinking"]
                | ["thinking_budget"]
                | ["reasoning_effort", "thinking_budget"]
                | ["enable_thinking", "thinking_budget"]
        ),
        ThinkingEncodingV1::ZaiOpenaiChat => {
            matches!(
                controls.as_slice(),
                ["thinking"] | ["reasoning_effort", "thinking"]
            )
        }
        _ => false,
    }
}

fn native_thinking_conflict(
    request: &ChatRequestV1,
    family: Family,
    encoding: Option<ThinkingEncodingV1>,
) -> Result<(), ProviderError> {
    let object = |bucket: &str| request.extensions.get(bucket).and_then(Value::as_object);
    let conflict = match family {
        Family::OpenaiChatCompletions => object("openai").is_some_and(|body| {
            encoding.is_none_or(|encoding| !openai_native_encoding_matches(body, request, encoding))
                && [
                    "reasoning_effort",
                    "enable_thinking",
                    "thinking_budget",
                    "thinking",
                ]
                .iter()
                .any(|key| body.get(*key).is_some_and(|value| !value.is_null()))
        }),
        Family::OpenaiResponses => object("openai_responses").is_some_and(|body| {
            body.get("reasoning")
                .is_some_and(|reasoning| !reasoning.is_null() && reasoning.as_object().is_none())
                || ((body
                    .get("reasoning_effort")
                    .is_some_and(|value| !value.is_null())
                    || body
                        .get("reasoning")
                        .and_then(Value::as_object)
                        .is_some_and(|value| {
                            value.get("effort").is_some_and(|effort| !effort.is_null())
                        }))
                    && (encoding != Some(ThinkingEncodingV1::OpenaiResponses)
                        || !responses_native_thinking_matches(body, request)))
        }),
        Family::AnthropicMessages => {
            object("anthropic").is_some_and(
                |body| match crate::thinking::decode_anthropic_thinking(body) {
                    Ok(None) => false,
                    Ok(native) => {
                        encoding != Some(ThinkingEncodingV1::AnthropicMessages)
                            || native != request.thinking
                    }
                    Err(_) => true,
                },
            )
        }
        Family::GeminiGenerateContent => object("gemini").is_some_and(|body| {
            let extras = crate::thinking::gemini_generation_config(body)
                .and_then(|config| {
                    config
                        .map(crate::thinking::gemini_thinking_config)
                        .transpose()
                })
                .is_ok_and(|config| {
                    config.flatten().is_some_and(|thinking| {
                        thinking
                            .keys()
                            .any(|key| !crate::thinking::is_gemini_thinking_field(key))
                    })
                });
            extras
                || match crate::thinking::decode_gemini_thinking(body) {
                    Ok(None) => false,
                    Ok(native) => {
                        encoding != Some(ThinkingEncodingV1::GeminiGenerateContent)
                            || native != request.thinking
                    }
                    Err(_) => true,
                }
        }),
        Family::CohereChat
        | Family::OllamaChat
        | Family::BedrockConverse
        | Family::HostCallback => false,
    };
    if conflict {
        return Err(invalid(
            "native thinking control was not admitted by exact model capability",
        ));
    }
    Ok(())
}

impl ProviderHandle {
    fn compile_thinking_request(
        &self,
        request: &ChatRequestV1,
    ) -> Result<Option<sandhi_core::chat::ThinkingEncodingV1>, ProviderError> {
        if request.compiled_thinking.is_some() {
            return Err(invalid(
                "thinking control was compiled outside the provider handle",
            ));
        }
        let capability = self.thinking_capability(&request.model)?;
        let encoding = capability.encoding;
        native_thinking_conflict(request, self.family().endpoint_family(), encoding)?;
        let Some(thinking) = &request.thinking else {
            return Ok(None);
        };
        capability.admits(thinking).map_err(invalid)?;
        let encoding = encoding.expect("admitted thinking carries an encoding");
        if encoding.endpoint_family() != self.family().endpoint_family() {
            return Err(invalid(
                "thinking capability encoding does not match the selected endpoint family",
            ));
        }
        Ok(Some(encoding))
    }

    pub fn apply_cache_plan(
        &self,
        request: &ChatRequestV1,
        body: &mut Value,
    ) -> Result<(), ProviderError> {
        if self.peer_role == sandhi_core::PeerRoleV1::SandhiGatewayV1 {
            return Err(invalid("gateway cache plans belong to the server"));
        }
        if let Some(plan) = &request.compiled_cache {
            if plan.binding_sha256 != self.cache_binding_sha256()? {
                return Err(invalid(
                    "cache plan belongs to another backend or credential generation",
                ));
            }
        }
        apply(request, self.family().endpoint_family(), body)
    }
    fn cache_binding_sha256(&self) -> Result<String, ProviderError> {
        let binding = self.backend_binding();
        cache_digest(&(
            self.family().endpoint_family(),
            self.transport_slug(),
            self.routing_credential_fingerprint(),
            binding.map(|b| {
                (
                    &b.id,
                    b.revision,
                    &b.credential_ref,
                    b.credential_generation,
                    &b.endpoint_profile,
                    &b.cache_policy,
                )
            }),
        ))
        .map_err(invalid)
    }

    pub fn compile_cache_plan(
        &self,
        request: &ChatRequestV1,
    ) -> Result<Option<Plan>, ProviderError> {
        if self.peer_role == sandhi_core::PeerRoleV1::SandhiGatewayV1 {
            return Err(invalid("gateway cache plans belong to the server"));
        }
        if let Some(plan) = &request.compiled_cache {
            if plan.binding_sha256 != self.cache_binding_sha256()? {
                return Err(invalid(
                    "cache plan belongs to another backend or credential generation",
                ));
            }
            validate_input(request, self.family().endpoint_family(), plan)?;
            return Ok(Some(plan.clone()));
        }
        let policy = self
            .backend_binding()
            .and_then(|binding| binding.cache_policy.as_ref());
        let default_intent = policy.map(|policy| {
            policy.default_intent.clone().unwrap_or(CacheIntentV1 {
                mode: Mode::PreserveNative,
                lifetime: None,
            })
        });
        if let (Some(policy), Some(intent)) = (policy, &request.cache_intent) {
            if !policy.allow_request_override && default_intent.as_ref() != Some(intent) {
                return Err(invalid(
                    "request cache intent conflicts with the backend's fixed policy",
                ));
            }
        }
        let scoped = policy.is_some_and(|policy| policy.namespace == Namespace::ScopedKey);
        let intent = request.cache_intent.clone().or(default_intent);
        let Some(intent) = intent else {
            return Ok(None);
        };
        intent.validate().map_err(invalid)?;
        let fallback = CacheCapabilitiesV1 {
            profile: Profile::NativeOnly,
            lifetimes: Vec::new(),
            max_breakpoints: None,
        };
        let capabilities = if let Some(policy) = policy {
            policy
                .models
                .iter()
                .find(|(model, _)| model.eq_ignore_ascii_case(&request.model))
                .map(|(_, caps)| caps)
                .ok_or_else(|| {
                    invalid("cache capabilities are not declared for this backend model")
                })?
        } else {
            &fallback
        };
        let family = self.family().endpoint_family();
        capabilities.validate(family).map_err(invalid)?;
        let automatic = matches!(
            intent.mode,
            Mode::Auto | Mode::AutoStablePrefix | Mode::AutoConversationPrefix
        );
        if (automatic || scoped) && !self.allows_cache_synthesis() {
            return Err(invalid(
                "cache synthesis is not qualified for this transport profile",
            ));
        }
        if scoped
            && !matches!(
                capabilities.profile,
                Profile::OpenaiLegacy | Profile::OpenaiExplicit
            )
        {
            return Err(invalid("this profile cannot express a scoped cache key"));
        }
        if automatic && capabilities.profile == Profile::NativeOnly {
            return Err(invalid(
                "automatic cache intent requires declared backend/model capabilities",
            ));
        }
        if intent
            .lifetime
            .is_some_and(|ttl| !capabilities.lifetimes.contains(&ttl))
        {
            return Err(invalid(
                "requested cache lifetime is not supported by this backend model",
            ));
        }
        if matches!(
            intent.mode,
            Mode::AutoStablePrefix | Mode::AutoConversationPrefix
        ) && !matches!(
            capabilities.profile,
            Profile::AnthropicBreakpoints | Profile::OpenaiExplicit
        ) {
            return Err(invalid(
                "required cache boundary cannot be expressed by this backend profile",
            ));
        }
        let mut plan = Plan {
            endpoint_family: family,
            model: request.model.clone(),
            binding_sha256: self.cache_binding_sha256()?,
            input_sha256: request.cache_input_sha256().map_err(invalid)?,
            intent,
            capability: capabilities.profile,
            native_preserved: false,
            scoped_key: None,
            mutations: Vec::new(),
        };
        // Preserve/suppress have no wire mutations unless the server independently
        // requires a scoped key. Avoid encoding an untouched native request.
        if !automatic && !scoped {
            plan.native_preserved = true;
            return Ok(Some(plan));
        }
        if plan.capability == Profile::ImplicitOnly {
            return Ok(Some(plan));
        }
        let mut probe = request.clone();
        probe.compiled_cache = None;
        probe.cache_intent = Some(plan.intent.clone()); // suppress legacy automatic placement
                                                        // A structural cache preflight is independent of the later admitted output bound.
        probe.max_output_tokens.get_or_insert(1);
        let body = match self.family() {
            ProviderFamily::OpenAiCompat => crate::typed::encode_openai_request_base(&probe)?,
            ProviderFamily::OpenAiResponses => {
                crate::openai_responses_typed::encode_responses_request_base(
                    &probe,
                    OpenAiResponsesProfile::Standard,
                )?
            }
            ProviderFamily::Anthropic => {
                crate::anthropic_typed::encode_anthropic_request_base(&probe)?
            }
            ProviderFamily::Gemini => crate::gemini_typed::encode_gemini_request_base(&probe)?,
            _ => {
                return Err(invalid(
                    "cache synthesis is unsupported by this endpoint family",
                ))
            }
        };
        if automatic {
            compile_placement(&body, capabilities, &mut plan)?;
        }
        if scoped {
            if !matches!(
                plan.capability,
                Profile::OpenaiLegacy | Profile::OpenaiExplicit
            ) {
                return Err(invalid("this profile cannot express a scoped cache key"));
            }
            let binding = self
                .backend_binding()
                .filter(|binding| binding.credential_generation > 0)
                .ok_or_else(|| {
                    invalid("scoped cache keys require a durable backend credential generation")
                })?;
            let scope = request
                .metadata
                .virtual_key_id
                .as_deref()
                .filter(|scope| !scope.is_empty() && scope.len() <= 512)
                .ok_or_else(|| {
                    invalid("scoped cache keys require an authenticated virtual-key scope")
                })?;
            let native_key = match body.get("prompt_cache_key") {
                Some(Value::String(key))
                    if !key.is_empty()
                        && key.len() <= 512
                        && !key.chars().any(char::is_control) =>
                {
                    Some(key.as_str())
                }
                Some(_) => {
                    return Err(invalid(
                        "native cache key must be a nonempty bounded string",
                    ))
                }
                None => None,
            };
            let hint = native_key
                .or(request.metadata.session_id.as_deref())
                .unwrap_or("");
            if hint.len() > 512 {
                return Err(invalid("cache namespace hint exceeds the supported length"));
            }
            let key = format!(
                "sandhi:{}",
                cache_digest(&(
                    "cache-namespace-v2",
                    self.routing_credential_fingerprint(),
                    scope,
                    &request.metadata.subject_id,
                    &request.metadata.group_id,
                    &binding.id,
                    binding.revision,
                    &binding.credential_ref,
                    binding.credential_generation,
                    &request.model,
                    &binding.endpoint_profile,
                    &binding.cache_policy,
                    hint
                ))
                .map_err(invalid)?
            );
            plan.scoped_key = Some(key.clone());
            plan.mutations.push(Mutation::TopLevel {
                field: "prompt_cache_key".into(),
                value: json!(key),
            });
        }
        Ok(Some(plan))
    }

    pub(crate) fn prepare_cache_request(
        &self,
        mut request: ChatRequestV1,
    ) -> Result<ChatRequestV1, ProviderError> {
        if self.peer_role == sandhi_core::PeerRoleV1::SandhiGatewayV1 {
            if request.compiled_cache.is_some() {
                return Err(invalid(
                    "gateway peer cannot accept a client-compiled cache plan",
                ));
            }
            request.validate().map_err(invalid)?;
            if request.compiled_thinking.is_some() {
                return Err(invalid(
                    "gateway peer cannot accept a client-compiled thinking control",
                ));
            }
            let family = self.family().endpoint_family();
            // A gateway peer knows the syntax of its immediate ingress dialect.
            // The server knows the routed model and owns its capability admission.
            let encoding = request
                .thinking
                .as_ref()
                .map(|thinking| {
                    if thinking.enabled
                        && thinking.effort.is_none()
                        && thinking.budget_tokens.is_none()
                        && family != Family::AnthropicMessages
                    {
                        return Err(invalid("explicit thinking enable requires a control"));
                    }
                    match family {
                        Family::OpenaiChatCompletions => {
                            Ok(openai_gateway_thinking_encoding(&request))
                        }
                        Family::OpenaiResponses => Ok(ThinkingEncodingV1::OpenaiResponses),
                        Family::AnthropicMessages => Ok(ThinkingEncodingV1::AnthropicMessages),
                        Family::GeminiGenerateContent => {
                            Ok(ThinkingEncodingV1::GeminiGenerateContent)
                        }
                        _ => Err(invalid(
                            "thinking is unsupported by this gateway ingress dialect",
                        )),
                    }
                })
                .transpose()?;
            native_thinking_conflict(&request, family, encoding)?;
            request.compiled_thinking = encoding;
            return Ok(request);
        }
        if self.family() == ProviderFamily::Ollama {
            if let Some(native) = request
                .extensions
                .get("ollama")
                .and_then(|body| body.get("think"))
                .filter(|value| !value.is_null())
            {
                let enabled = native
                    .as_bool()
                    .ok_or_else(|| invalid("Ollama native think must be a Boolean"))?;
                if request.thinking.is_none() {
                    use sandhi_core::chat::ThinkingRequestV1;
                    request.thinking = Some(if enabled {
                        ThinkingRequestV1::enabled()
                    } else {
                        ThinkingRequestV1::disabled()
                    });
                }
            }
        }
        request.compiled_thinking = self.compile_thinking_request(&request)?;
        request.compiled_cache = self.compile_cache_plan(&request)?;
        Ok(request)
    }
}

fn validate_input(
    request: &ChatRequestV1,
    family: Family,
    plan: &Plan,
) -> Result<(), ProviderError> {
    if family != plan.endpoint_family
        || request.model != plan.model
        || request.cache_input_sha256().map_err(invalid)? != plan.input_sha256
    {
        return Err(invalid(
            "cache plan no longer matches the admitted target or input",
        ));
    }
    Ok(())
}

pub(crate) fn apply(
    request: &ChatRequestV1,
    family: Family,
    body: &mut Value,
) -> Result<(), ProviderError> {
    let Some(plan) = &request.compiled_cache else {
        if let Some(intent) = &request.cache_intent {
            intent.validate().map_err(invalid)?;
            if matches!(
                intent.mode,
                Mode::Auto | Mode::AutoStablePrefix | Mode::AutoConversationPrefix
            ) {
                return Err(invalid(
                    "automatic cache intent requires a handle-compiled cache plan",
                ));
            }
        }
        return Ok(());
    };
    validate_input(request, family, plan)?;
    if plan.mutations.len() > 6 {
        return Err(invalid("cache plan exceeds the mutation bound"));
    }
    for mutation in &plan.mutations {
        match mutation {
            Mutation::TopLevel { field, value } => {
                if !matches!(
                    family,
                    Family::OpenaiResponses | Family::OpenaiChatCompletions
                ) || !matches!(
                    field.as_str(),
                    "prompt_cache_options" | "prompt_cache_retention" | "prompt_cache_key"
                ) {
                    return Err(invalid(
                        "cache plan contains an incompatible top-level control",
                    ));
                }
                body.as_object_mut()
                    .ok_or_else(|| invalid("cache patch requires a native object"))?
                    .insert(field.clone(), value.clone());
            }
            Mutation::Marker {
                pointer,
                field,
                value,
            } => {
                if !matches!(
                    (family, field.as_str()),
                    (Family::AnthropicMessages, "cache_control")
                        | (Family::OpenaiResponses, "prompt_cache_breakpoint")
                ) {
                    return Err(invalid("cache plan contains an incompatible block control"));
                }
                let block = body
                    .pointer_mut(pointer)
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| invalid("cache boundary no longer exists"))?;
                if block.contains_key(field) {
                    return Err(invalid("cache plan would replace a native block control"));
                }
                block.insert(field.clone(), value.clone());
            }
            Mutation::AnthropicTextString { pointer, value } => {
                if family != Family::AnthropicMessages {
                    return Err(invalid("incompatible Anthropic cache boundary"));
                }
                let block = body
                    .pointer_mut(pointer)
                    .ok_or_else(|| invalid("cache text boundary no longer exists"))?;
                if !block.is_string() {
                    return Err(invalid("cache text boundary changed shape"));
                }
                let text = std::mem::take(block);
                *block = json!([{"type":"text","text":text,"cache_control":value}]);
            }
            Mutation::ResponsesTextString { pointer, value } => {
                if family != Family::OpenaiResponses {
                    return Err(invalid("incompatible Responses cache boundary"));
                }
                let block = body
                    .pointer_mut(pointer)
                    .ok_or_else(|| invalid("cache text boundary no longer exists"))?;
                if !block.is_string() {
                    return Err(invalid("cache text boundary changed shape"));
                }
                let text = std::mem::take(block);
                *block = json!([{"type":"input_text","text":text,"prompt_cache_breakpoint":value}]);
            }
        }
    }
    Ok(())
}

fn array(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}
fn nonempty_text(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !text.is_empty())
}

/// Only protocol-defined control locations. JSON schemas and tool arguments may
/// legitimately contain keys named cache_control; those are never directives.
pub(crate) fn native_controls(body: &Value, family: Family) -> Vec<&Value> {
    let mut controls = Vec::new();
    let fields: &[&str] = match family {
        Family::AnthropicMessages => &[], // automatic top-level boundary follows block boundaries
        Family::OpenaiChatCompletions | Family::OpenaiResponses => {
            &["prompt_cache_options", "prompt_cache_retention"]
        }
        Family::GeminiGenerateContent => &["cachedContent"],
        _ => &[],
    };
    for field in fields {
        if let Some(value) = body.get(*field) {
            controls.push(value);
        }
    }
    let marker = if family == Family::AnthropicMessages {
        "cache_control"
    } else {
        "prompt_cache_breakpoint"
    };
    if family == Family::AnthropicMessages {
        for item in array(body.get("tools"))
            .iter()
            .chain(array(body.get("system")))
        {
            if let Some(value) = item.get(marker) {
                controls.push(value);
                if controls.len() >= 6 {
                    return controls;
                }
            }
        }
    }
    let items = if family == Family::OpenaiResponses {
        array(body.get("input"))
    } else {
        array(body.get("messages"))
    };
    for item in items {
        let parts = if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
            array(item.get("output"))
        } else {
            array(item.get("content"))
        };
        for part in parts {
            if let Some(value) = part.get(marker) {
                controls.push(value);
                if controls.len() >= 6 {
                    return controls;
                }
            }
        }
    }
    if family == Family::AnthropicMessages {
        if let Some(value) = body.get("cache_control") {
            controls.push(value);
        }
    }
    controls
}

pub fn contains_native_cache_controls(body: &Value, family: Family) -> bool {
    !native_controls(body, family).is_empty()
        || (matches!(
            family,
            Family::OpenaiChatCompletions | Family::OpenaiResponses
        ) && body.get("prompt_cache_key").is_some())
}

fn compile_placement(
    body: &Value,
    caps: &CacheCapabilitiesV1,
    plan: &mut Plan,
) -> Result<(), ProviderError> {
    let controls = native_controls(body, plan.endpoint_family);
    if !controls.is_empty() {
        validate_native_controls(body, caps, plan, &controls)?;
        plan.native_preserved = true;
        return Ok(());
    }
    match caps.profile {
        Profile::AnthropicBreakpoints => {
            let mut marker = json!({"type":"ephemeral"});
            if let Some(ttl) = plan.intent.lifetime {
                marker["ttl"] = json!(ttl.as_str());
            }
            let (pointer, wrap) = if plan.intent.mode == Mode::AutoStablePrefix {
                if let Some(system) = body.get("system").filter(|value| !value.is_null()) {
                    if nonempty_text(system) {
                        ("/system".into(), true)
                    } else {
                        let blocks = array(Some(system));
                        let index = blocks.len().checked_sub(1).ok_or_else(|| {
                            invalid("stable cache intent requires a nonempty system or tool prefix")
                        })?;
                        if !cacheable_anthropic(&blocks[index]) {
                            return Err(invalid(
                                "last system block cannot carry an automatic cache boundary",
                            ));
                        }
                        (format!("/system/{index}"), false)
                    }
                } else {
                    let tools = array(body.get("tools"));
                    let index = tools.len().checked_sub(1).ok_or_else(|| {
                        invalid("stable cache intent requires a nonempty system or tool prefix")
                    })?;
                    (format!("/tools/{index}"), false)
                }
            } else {
                let messages = array(body.get("messages"));
                let message = messages
                    .len()
                    .checked_sub(1)
                    .ok_or_else(|| invalid("conversation cache intent requires a message"))?;
                let content = messages[message]
                    .get("content")
                    .ok_or_else(|| invalid("conversation cache boundary requires content"))?;
                if nonempty_text(content) {
                    (format!("/messages/{message}/content"), true)
                } else {
                    let blocks = array(Some(content));
                    let block = blocks.len().checked_sub(1).ok_or_else(|| {
                        invalid("conversation cache boundary requires nonempty content")
                    })?;
                    if !cacheable_anthropic(&blocks[block]) {
                        return Err(invalid(
                            "last message block cannot carry an automatic cache boundary",
                        ));
                    }
                    (format!("/messages/{message}/content/{block}"), false)
                }
            };
            plan.mutations.push(if wrap {
                Mutation::AnthropicTextString {
                    pointer,
                    value: marker,
                }
            } else {
                Mutation::Marker {
                    pointer,
                    field: "cache_control".into(),
                    value: marker,
                }
            });
        }
        Profile::OpenaiExplicit => {
            let explicit = plan.intent.mode == Mode::AutoStablePrefix;
            let mut options = json!({"mode":if explicit {"explicit"} else {"implicit"}});
            if let Some(ttl) = plan.intent.lifetime {
                options["ttl"] = json!(ttl.as_str());
            }
            if explicit {
                if body
                    .get("instructions")
                    .is_some_and(|value| !value.is_null())
                {
                    return Err(invalid("explicit stable caching requires materialized developer/system input; top-level instructions cannot contain a breakpoint"));
                }
                let items = array(body.get("input"));
                let index = items
                    .iter()
                    .take_while(|item| {
                        matches!(
                            item.get("role").and_then(Value::as_str),
                            Some("system" | "developer")
                        )
                    })
                    .count()
                    .checked_sub(1)
                    .ok_or_else(|| {
                        invalid(
                            "explicit stable caching requires an initial developer/system message",
                        )
                    })?;
                let content = items[index]
                    .get("content")
                    .ok_or_else(|| invalid("stable Responses boundary requires content"))?;
                let marker = json!({"mode":"explicit"});
                if nonempty_text(content) {
                    plan.mutations.push(Mutation::ResponsesTextString {
                        pointer: format!("/input/{index}/content"),
                        value: marker,
                    });
                } else {
                    let parts = array(Some(content));
                    let part = parts.len().checked_sub(1).ok_or_else(|| {
                        invalid("stable Responses boundary requires nonempty content")
                    })?;
                    if parts[part].get("type").and_then(Value::as_str) != Some("input_text")
                        || !parts[part].get("text").is_some_and(nonempty_text)
                    {
                        return Err(invalid("automatic stable Responses boundary currently requires an input_text block"));
                    }
                    plan.mutations.push(Mutation::Marker {
                        pointer: format!("/input/{index}/content/{part}"),
                        field: "prompt_cache_breakpoint".into(),
                        value: marker,
                    });
                }
            }
            plan.mutations.push(Mutation::TopLevel {
                field: "prompt_cache_options".into(),
                value: options,
            });
        }
        Profile::OpenaiLegacy => {
            if let Some(ttl) = plan.intent.lifetime {
                plan.mutations.push(Mutation::TopLevel {
                    field: "prompt_cache_retention".into(),
                    value: json!(ttl.as_str()),
                });
            }
        }
        Profile::ImplicitOnly => {}
        Profile::NativeOnly => {
            return Err(invalid("cache synthesis is not declared for this model"))
        }
    }
    Ok(())
}

fn cacheable_anthropic(block: &Value) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block.get("text").is_some_and(nonempty_text),
        Some("image" | "document" | "tool_use" | "tool_result") => true,
        _ => false,
    }
}

fn validate_native_controls(
    body: &Value,
    caps: &CacheCapabilitiesV1,
    plan: &Plan,
    controls: &[&Value],
) -> Result<(), ProviderError> {
    let slots = if caps.profile == Profile::OpenaiExplicit {
        let options = usize::from(body.get("prompt_cache_options").is_some());
        let implicit = usize::from(
            body.pointer("/prompt_cache_options/mode")
                .and_then(Value::as_str)
                != Some("explicit"),
        );
        controls.len().saturating_sub(options) + implicit
    } else {
        controls.len()
    };
    if caps
        .max_breakpoints
        .is_some_and(|limit| slots > usize::from(limit))
    {
        return Err(invalid(
            "native cache controls exceed the declared breakpoint limit",
        ));
    }
    let mut previous_rank = u8::MAX;
    for control in controls {
        if control.get("ttl").is_some_and(|ttl| !ttl.is_string()) {
            return Err(invalid("native cache lifetime must be a supported string"));
        }
        let ttl = match caps.profile {
            Profile::AnthropicBreakpoints => {
                if control.get("type").and_then(Value::as_str) != Some("ephemeral") {
                    return Err(invalid("unsupported native Anthropic cache control"));
                }
                let ttl = control.get("ttl").and_then(Value::as_str).unwrap_or("5m");
                let rank = match ttl {
                    "1h" => 2,
                    "5m" => 1,
                    _ => return Err(invalid("unsupported native Anthropic cache lifetime")),
                };
                if rank > previous_rank {
                    return Err(invalid(
                        "longer Anthropic cache lifetimes must precede shorter lifetimes",
                    ));
                }
                previous_rank = rank;
                ttl
            }
            Profile::OpenaiLegacy => control
                .as_str()
                .ok_or_else(|| invalid("native cache controls do not match the legacy profile"))?,
            Profile::OpenaiExplicit => {
                if !matches!(
                    control.get("mode").and_then(Value::as_str),
                    Some("implicit" | "explicit")
                ) {
                    return Err(invalid("unsupported native OpenAI cache control"));
                }
                control
                    .get("ttl")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        body.pointer("/prompt_cache_options/ttl")
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("30m")
            }
            _ => return Err(invalid(
                "native explicit cache control is incompatible with the declared automatic profile",
            )),
        };
        let explicit_ttl = control.get("ttl").is_some()
            || (caps.profile == Profile::OpenaiExplicit
                && body.pointer("/prompt_cache_options/ttl").is_some())
            || caps.profile == Profile::OpenaiLegacy;
        if (explicit_ttl && !caps.lifetimes.iter().any(|allowed| allowed.as_str() == ttl))
            || plan
                .intent
                .lifetime
                .is_some_and(|required| required.as_str() != ttl)
        {
            return Err(invalid(
                "native cache lifetime conflicts with requested or declared capabilities",
            ));
        }
    }
    Ok(())
}

/// Gateway requests carry intent to the selected backend's compiler. No local plan,
/// capability declaration, automatic marker or namespace crosses this hop.
pub(crate) fn encode_for_peer(
    request: &ChatRequestV1,
    family: Family,
    role: sandhi_core::PeerRoleV1,
    base: impl FnOnce(&ChatRequestV1) -> Result<Value, ProviderError>,
) -> Result<Value, ProviderError> {
    if role == sandhi_core::PeerRoleV1::Provider {
        let mut body = base(request)?;
        apply(request, family, &mut body)?;
        return Ok(body);
    }
    if request.compiled_cache.is_some() {
        return Err(invalid(
            "gateway peer cannot accept a client-compiled cache plan",
        ));
    }
    let mut input = request.clone();
    // Suppress the historical encoder's placement even when intent is omitted.
    input.cache_intent = Some(CacheIntentV1 {
        mode: Mode::SuppressGateway,
        lifetime: None,
    });
    let mut body = base(&input)?;
    let object = body
        .as_object_mut()
        .ok_or_else(|| invalid("gateway request requires an object"))?;
    if family == Family::OpenaiChatCompletions
        && request
            .native_input
            .as_ref()
            .is_some_and(|native| native.endpoint_family == family)
    {
        // The SDK decoder separates the reserved target buckets from the
        // OpenAI body. Restore them for the gateway that owns backend admission.
        let extensions: Map<String, Value> = request
            .extensions
            .iter()
            .filter(|(name, _)| name.as_str() != "openai")
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        if !extensions.is_empty() {
            object.insert("sandhi_extensions".into(), Value::Object(extensions));
        }
    }
    if object.contains_key("sandhi_cache") {
        return Err(invalid(
            "gateway cache intent must use the typed cache_intent field",
        ));
    }
    if let Some(intent) = &request.cache_intent {
        intent.validate().map_err(invalid)?;
        object.insert(
            "sandhi_cache".into(),
            serde_json::to_value(intent).map_err(|_| invalid("invalid gateway cache intent"))?,
        );
    }
    Ok(body)
}

#[cfg(test)]
mod thinking_admission_tests {
    use super::*;
    use sandhi_core::chat::{ThinkingCapabilityV1, ThinkingEncodingV1, ThinkingRequestV1};
    use sandhi_core::{EndpointFamilyV1, ModelDescriptorV1};

    fn bound_qwen(capability: ThinkingCapabilityV1) -> ProviderHandle {
        let descriptor: ModelDescriptorV1 = serde_json::from_value(serde_json::json!({
            "id":"qwen-fixture",
            "thinking":capability
        }))
        .unwrap();
        crate::ProviderRuntime::new()
            .openai_compat(
                "qwen",
                "http://127.0.0.1:1/v1",
                "synthetic",
                http::HeaderMap::new(),
                Some(0),
                None,
                None,
                None,
            )
            .with_backend_binding(crate::typed::BackendBinding {
                id: "qwen".into(),
                revision: 1,
                provider: "qwen".into(),
                credential_ref: "qwen:fixture".into(),
                credential_generation: 1,
                endpoint_family: EndpointFamilyV1::OpenaiChatCompletions,
                endpoint_profile: "standard".into(),
                models: vec![descriptor],
                cache_policy: None,
            })
    }

    fn request(thinking: ThinkingRequestV1) -> ChatRequestV1 {
        serde_json::from_value(serde_json::json!({
            "model":"qwen-fixture",
            "messages":[{"role":"user","content":"hello"}],
            "thinking":thinking
        }))
        .unwrap()
    }

    fn bound_responses(capability: ThinkingCapabilityV1) -> ProviderHandle {
        let descriptor: ModelDescriptorV1 = serde_json::from_value(serde_json::json!({
            "id":"responses-fixture",
            "thinking":capability
        }))
        .unwrap();
        crate::ProviderRuntime::new()
            .openai_responses(
                "openai",
                "http://127.0.0.1:1/v1",
                "synthetic",
                http::HeaderMap::new(),
                Some(0),
                None,
                None,
                None,
            )
            .with_backend_binding(crate::typed::BackendBinding {
                id: "responses".into(),
                revision: 1,
                provider: "openai".into(),
                credential_ref: "openai:fixture".into(),
                credential_generation: 1,
                endpoint_family: EndpointFamilyV1::OpenaiResponses,
                endpoint_profile: "standard".into(),
                models: vec![descriptor],
                cache_policy: None,
            })
    }

    #[test]
    fn explicit_thinking_fails_closed_for_unknown_models_before_encoder_egress() {
        let provider = crate::ProviderRuntime::new().openai_compat(
            "moonshot",
            "http://127.0.0.1:1/v1",
            "synthetic",
            http::HeaderMap::new(),
            Some(0),
            None,
            None,
            None,
        );
        let mut request = request(ThinkingRequestV1::enabled().with_effort("medium"));
        request.model = "not-in-any-catalog".into();
        assert!(provider.preflight_request(&request).is_err());
        assert!(provider.encode_execution_request(&request).is_err());
    }

    #[test]
    fn gateway_peer_encodes_thinking_for_a_server_owned_model_alias() {
        let gateway =
            crate::ProviderRuntime::new().gateway_peer(crate::typed::ProviderTransportConfig::new(
                ProviderFamily::OpenAiCompat,
                "openai",
                "http://127.0.0.1:1/v1",
                "synthetic",
            ));
        for (control, wire) in [
            (ThinkingRequestV1::enabled().with_effort("medium"), "medium"),
            (ThinkingRequestV1::disabled(), "none"),
        ] {
            let request = request(control);
            assert!(gateway.model_descriptor(&request.model).is_none());
            assert_eq!(
                gateway.encode_execution_request(&request).unwrap()["reasoning_effort"],
                wire
            );
        }
    }

    #[test]
    fn gateway_peer_preserves_sdk_chat_thinking_spellings_for_server_admission() {
        let gateway =
            crate::ProviderRuntime::new().gateway_peer(crate::typed::ProviderTransportConfig::new(
                ProviderFamily::OpenAiCompat,
                "openai",
                "http://127.0.0.1:1/v1",
                "synthetic",
            ));
        for (key, value) in [
            ("thinking_budget", json!(2048)),
            ("thinking", json!({"type":"enabled","budget_tokens":2048})),
        ] {
            let mut request = request(ThinkingRequestV1::enabled().with_budget(2048));
            let mut body = json!({"messages":[{"role":"user","content":"hello"}]});
            body[key] = value.clone();
            request.extensions.insert("openai".into(), body);
            request
                .capture_native_input(EndpointFamilyV1::OpenaiChatCompletions, vec![])
                .unwrap();
            assert_eq!(
                gateway.encode_execution_request(&request).unwrap()[key],
                value
            );
        }
    }

    #[test]
    fn native_thinking_control_cannot_bypass_exact_admission() {
        let provider = bound_qwen(ThinkingCapabilityV1::unsupported());
        let mut request = request(ThinkingRequestV1::disabled());
        request.thinking = None;
        request.extensions.insert(
            "openai".into(),
            serde_json::json!({"thinking_budget": 16_384}),
        );
        let error = provider.prepare_cache_request(request).unwrap_err();
        assert!(error
            .to_string()
            .contains("native thinking control was not admitted"));
    }

    #[test]
    fn explicit_null_vendor_thinking_controls_mean_default() {
        let provider = bound_qwen(ThinkingCapabilityV1::unsupported());
        let mut request = request(ThinkingRequestV1::disabled());
        request.thinking = None;
        request.extensions.insert(
            "openai".into(),
            serde_json::json!({
                "reasoning_effort": serde_json::Value::Null,
                "enable_thinking": serde_json::Value::Null,
                "thinking_budget": serde_json::Value::Null
            }),
        );
        assert!(provider.prepare_cache_request(request.clone()).is_ok());
    }

    #[test]
    fn gemini_null_thinking_config_reaches_backend_preflight_as_default() {
        let descriptor: ModelDescriptorV1 = serde_json::from_value(serde_json::json!({
            "id":"gemini-fixture",
            "thinking":ThinkingCapabilityV1::unsupported()
        }))
        .unwrap();
        let provider = crate::ProviderRuntime::new()
            .gemini(
                "http://127.0.0.1:1/v1beta",
                "synthetic",
                crate::GeminiAuthScheme::ApiKey,
                http::HeaderMap::new(),
                Some(0),
                None,
                None,
                None,
            )
            .with_backend_binding(crate::typed::BackendBinding {
                id: "gemini".into(),
                revision: 1,
                provider: "gemini".into(),
                credential_ref: "gemini:fixture".into(),
                credential_generation: 1,
                endpoint_family: EndpointFamilyV1::GeminiGenerateContent,
                endpoint_profile: "standard".into(),
                models: vec![descriptor],
                cache_policy: None,
            });
        let mut request = request(ThinkingRequestV1::disabled());
        request.model = "gemini-fixture".into();
        request.thinking = None;
        request.extensions.insert(
            "gemini".into(),
            serde_json::json!({
                "contents":[{"role":"user","parts":[{"text":"hello"}]}],
                "generationConfig":{"thinkingConfig":serde_json::Value::Null}
            }),
        );
        assert!(provider.prepare_cache_request(request.clone()).is_ok());
    }

    #[test]
    fn admitted_openai_native_thinking_survives_backend_preflight() {
        let capability =
            ThinkingCapabilityV1::effort(ThinkingEncodingV1::OpenaiChat, &["low", "high"], None);
        let provider = bound_qwen(capability);
        let mut request = request(ThinkingRequestV1::enabled().with_effort("high"));
        request.extensions.insert(
            "openai".into(),
            serde_json::json!({"reasoning_effort":"high"}),
        );
        assert!(provider.prepare_cache_request(request).is_ok());
    }

    #[test]
    fn admitted_qwen_disable_and_budget_controls_survive_backend_preflight() {
        let mut capability =
            ThinkingCapabilityV1::budget(ThinkingEncodingV1::QwenOpenaiChat, 1024, 131_072);
        capability.can_disable = true;
        let provider = bound_qwen(capability);

        let mut disabled = request(ThinkingRequestV1::disabled());
        disabled.extensions.insert(
            "openai".into(),
            serde_json::json!({"enable_thinking":false}),
        );
        assert!(provider.prepare_cache_request(disabled).is_ok());

        let mut budget = request(ThinkingRequestV1::enabled().with_budget(16_384));
        budget.extensions.insert(
            "openai".into(),
            serde_json::json!({"enable_thinking":true,"thinking_budget":16_384}),
        );
        assert!(provider.prepare_cache_request(budget).is_ok());
    }

    #[test]
    fn admitted_zai_native_thinking_object_survives_backend_preflight() {
        let capability =
            ThinkingCapabilityV1::budget(ThinkingEncodingV1::ZaiOpenaiChat, 1024, 131_072);
        let provider = bound_qwen(capability);
        let mut request = request(ThinkingRequestV1::enabled().with_budget(16_384));
        request.extensions.insert(
            "openai".into(),
            serde_json::json!({"thinking":{"type":"enabled","budget_tokens":16_384}}),
        );
        assert!(provider.prepare_cache_request(request).is_ok());
    }

    #[test]
    fn exact_qwen_and_zai_thinking_contracts_are_enforced_before_upstream() {
        let mut qwen = ThinkingCapabilityV1::budget(ThinkingEncodingV1::QwenOpenaiChat, 1, 262_144);
        qwen.can_disable = true;
        let qwen = bound_qwen(qwen);

        let mut omitted = request(ThinkingRequestV1::disabled());
        omitted.thinking = None;
        assert!(qwen.prepare_cache_request(omitted.clone()).is_ok());
        assert!(qwen
            .encode_execution_request(&omitted)
            .unwrap()
            .get("enable_thinking")
            .is_none());

        let disabled = request(ThinkingRequestV1::disabled());
        assert!(qwen.prepare_cache_request(disabled.clone()).is_ok());
        assert_eq!(
            qwen.encode_execution_request(&disabled).unwrap()["enable_thinking"],
            false
        );

        assert!(qwen
            .prepare_cache_request(request(ThinkingRequestV1::enabled().with_budget(262_144)))
            .is_ok());
        assert!(qwen
            .prepare_cache_request(request(ThinkingRequestV1::enabled().with_budget(0)))
            .is_err());
        assert!(qwen
            .prepare_cache_request(request(ThinkingRequestV1::enabled().with_budget(262_145)))
            .is_err());
        assert!(qwen
            .prepare_cache_request(request(ThinkingRequestV1::enabled().with_effort("medium")))
            .is_err());

        let zai = ThinkingCapabilityV1::effort(
            ThinkingEncodingV1::ZaiOpenaiChat,
            &["low", "high", "max"],
            Some("max"),
        );
        let zai = bound_qwen(zai);
        for effort in ["low", "high", "max"] {
            assert!(zai
                .prepare_cache_request(request(ThinkingRequestV1::enabled().with_effort(effort)))
                .is_ok());
        }
        assert!(zai
            .prepare_cache_request(request(ThinkingRequestV1::disabled()))
            .is_err());
        assert!(zai
            .prepare_cache_request(request(ThinkingRequestV1::enabled().with_budget(16_384)))
            .is_err());
    }

    #[test]
    fn admitted_zai_effort_with_native_shape_survives_backend_preflight() {
        let capability = ThinkingCapabilityV1::effort(
            ThinkingEncodingV1::ZaiOpenaiChat,
            &["low", "high", "max"],
            Some("max"),
        );
        let provider = bound_qwen(capability);
        let mut request = request(ThinkingRequestV1::enabled().with_effort("low"));
        request.extensions.insert(
            "openai".into(),
            serde_json::json!({"thinking":{"type":"enabled"},"reasoning_effort":"low"}),
        );
        assert!(provider.prepare_cache_request(request).is_ok());
    }

    #[test]
    fn qualified_thinking_encoding_is_enforced_before_transparent_forwarding() {
        let zai = ThinkingCapabilityV1::budget(ThinkingEncodingV1::ZaiOpenaiChat, 1024, 131_072);
        let provider = bound_qwen(zai);
        let mut qwen_spelling = request(ThinkingRequestV1::disabled());
        qwen_spelling.extensions.insert(
            "openai".into(),
            serde_json::json!({"enable_thinking":false}),
        );
        assert!(provider.prepare_cache_request(qwen_spelling).is_err());

        let qwen = ThinkingCapabilityV1::budget(ThinkingEncodingV1::QwenOpenaiChat, 1024, 131_072);
        let provider = bound_qwen(qwen);
        let mut zai_spelling = request(ThinkingRequestV1::disabled());
        zai_spelling.extensions.insert(
            "openai".into(),
            serde_json::json!({"thinking":{"type":"disabled"}}),
        );
        assert!(provider.prepare_cache_request(zai_spelling).is_err());
    }

    #[test]
    fn admitted_responses_effort_survives_backend_preflight() {
        let capability = ThinkingCapabilityV1::effort(
            ThinkingEncodingV1::OpenaiResponses,
            &["low", "high"],
            None,
        );
        let provider = bound_responses(capability);
        let mut request = request(ThinkingRequestV1::enabled().with_effort("high"));
        request.model = "responses-fixture".into();
        request.extensions.insert(
            "openai_responses".into(),
            serde_json::json!({"reasoning":{"effort":"high","summary":"auto"}}),
        );
        assert!(provider.prepare_cache_request(request.clone()).is_ok());

        request.extensions.insert(
            "openai_responses".into(),
            serde_json::json!({"reasoning_effort":"high"}),
        );
        assert!(provider.prepare_cache_request(request).is_ok());
    }

    #[test]
    fn qwen_budget_only_model_admits_budget_and_rejects_effort() {
        let capability =
            ThinkingCapabilityV1::budget(ThinkingEncodingV1::QwenOpenaiChat, 1024, 131_072);
        let provider = bound_qwen(capability);
        let budget = ThinkingRequestV1::enabled().with_budget(16_384);
        let prepared = provider
            .prepare_cache_request(request(budget.clone()))
            .unwrap();
        assert_eq!(
            prepared.compiled_thinking,
            Some(ThinkingEncodingV1::QwenOpenaiChat)
        );
        let body = provider.encode_execution_request(&request(budget)).unwrap();
        assert_eq!(body["thinking_budget"], 16_384);
        assert!(body.get("reasoning_effort").is_none());

        let effort = ThinkingRequestV1::enabled().with_effort("medium");
        assert!(provider.prepare_cache_request(request(effort)).is_err());
    }

    #[test]
    fn qwen_effort_capability_is_exact_and_has_no_family_aliases() {
        let capability = ThinkingCapabilityV1::effort(
            ThinkingEncodingV1::QwenOpenaiChat,
            &["low", "medium", "xhigh"],
            Some("medium"),
        );
        let provider = bound_qwen(capability);
        let admitted = ThinkingRequestV1::enabled().with_effort("medium");
        let body = provider
            .encode_execution_request(&request(admitted))
            .unwrap();
        assert_eq!(body["reasoning_effort"], "medium");

        for alias in ["high", "max"] {
            let rejected = ThinkingRequestV1::enabled().with_effort(alias);
            assert!(
                provider.prepare_cache_request(request(rejected)).is_err(),
                "alias {alias} must not be admitted"
            );
        }
    }

    #[test]
    fn qwen_can_combine_exact_effort_with_admitted_budget() {
        let mut capability = ThinkingCapabilityV1::effort(
            ThinkingEncodingV1::QwenOpenaiChat,
            &["low", "medium", "xhigh"],
            None,
        );
        capability.budget_tokens = Some(sandhi_core::chat::ThinkingBudgetCapabilityV1 {
            min: 1024,
            max: 131_072,
        });
        capability.allow_effort_and_budget = true;
        let provider = bound_qwen(capability);
        let thinking = ThinkingRequestV1::enabled()
            .with_effort("medium")
            .with_budget(16_384);
        let mut native = request(thinking.clone());
        native.extensions.insert(
            "openai".into(),
            serde_json::json!({
                "reasoning_effort":"medium",
                "thinking_budget":16_384
            }),
        );
        assert!(provider.prepare_cache_request(native).is_ok());
        let body = provider
            .encode_execution_request(&request(thinking))
            .unwrap();
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["thinking_budget"], 16_384);
    }

    #[test]
    fn disable_requires_model_and_encoding_capability() {
        let no_disable =
            ThinkingCapabilityV1::budget(ThinkingEncodingV1::QwenOpenaiChat, 1024, 131_072);
        let provider = bound_qwen(no_disable);
        assert!(provider
            .prepare_cache_request(request(ThinkingRequestV1::disabled()))
            .is_err());

        let mut can_disable =
            ThinkingCapabilityV1::budget(ThinkingEncodingV1::QwenOpenaiChat, 1024, 131_072);
        can_disable.can_disable = true;
        let provider = bound_qwen(can_disable);
        let body = provider
            .encode_execution_request(&request(ThinkingRequestV1::disabled()))
            .unwrap();
        assert_eq!(body["enable_thinking"], false);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking_budget").is_none());
    }

    #[test]
    fn provider_compiled_thinking_is_private_and_family_checked() {
        let capability =
            ThinkingCapabilityV1::budget(ThinkingEncodingV1::QwenOpenaiChat, 1024, 131_072);
        let provider = bound_qwen(capability);
        let mut external = request(ThinkingRequestV1::enabled().with_budget(16_384));
        external.compiled_thinking = Some(ThinkingEncodingV1::QwenOpenaiChat);
        assert!(provider.prepare_cache_request(external).is_err());

        let mismatch =
            ThinkingCapabilityV1::budget(ThinkingEncodingV1::AnthropicMessages, 1024, 131_072);
        let provider = bound_qwen(mismatch);
        assert!(provider
            .prepare_cache_request(request(ThinkingRequestV1::enabled().with_budget(16_384)))
            .is_err());
    }
}

#[cfg(test)]
mod dialect_cache_tests {
    use super::*;
    use sandhi_core::chat::{ThinkingEncodingV1, ThinkingRequestV1};

    fn request(
        extensions: serde_json::Value,
        thinking: Option<ThinkingRequestV1>,
        encoding: Option<ThinkingEncodingV1>,
    ) -> ChatRequestV1 {
        let mut request: ChatRequestV1 = serde_json::from_value(serde_json::json!({
            "model": "cache-model",
            "messages": [{"role": "user", "content": "stable prefix"}],
            "extensions": extensions
        }))
        .unwrap();
        request.thinking = thinking;
        request.compiled_thinking = encoding;
        request
    }

    /// A thinking change is a generation-policy change, not a prompt change.
    /// Keeping it out of the cache input digest preserves a stable prefix hit
    /// instead of silently turning every reasoning-level change into a cold read.
    #[test]
    fn thinking_changes_do_not_invalidate_the_cache_input_digest() {
        let base = request(serde_json::json!({}), None, None);
        let effort = request(
            serde_json::json!({}),
            Some(ThinkingRequestV1::enabled().with_effort("medium")),
            Some(ThinkingEncodingV1::QwenOpenaiChat),
        );
        let budget = request(
            serde_json::json!({}),
            Some(ThinkingRequestV1::enabled().with_budget(16_384)),
            Some(ThinkingEncodingV1::AnthropicMessages),
        );
        let expected = base.cache_input_sha256().unwrap();
        assert_eq!(effort.cache_input_sha256().unwrap(), expected);
        assert_eq!(budget.cache_input_sha256().unwrap(), expected);
    }

    /// Each source dialect owns different cache spellings. Qualified thinking
    /// restoration must remove only thinking controls, never these cost levers.
    #[test]
    fn qualified_thinking_preserves_native_cache_controls_in_every_dialect() {
        let mut openai = request(
            serde_json::json!({
                "openai": {
                    "prompt_cache_options": {"mode": "on"},
                    "prompt_cache_retention": "24h",
                    "prompt_cache_key": "stable-key"
                }
            }),
            Some(ThinkingRequestV1::enabled().with_effort("medium")),
            Some(ThinkingEncodingV1::OpenaiChat),
        );
        openai.extensions.insert(
            "openai".into(),
            serde_json::json!({
                "prompt_cache_options": {"mode": "on"},
                "prompt_cache_retention": "24h",
                "prompt_cache_key": "stable-key"
            }),
        );
        let body = crate::typed::encode_openai_request(&openai).unwrap();
        assert_eq!(body["prompt_cache_options"], json!({"mode":"on"}));
        assert_eq!(body["prompt_cache_retention"], "24h");
        assert_eq!(body["prompt_cache_key"], "stable-key");

        let responses = request(
            serde_json::json!({
                "openai_responses": {
                    "prompt_cache_key": "responses-key",
                    "reasoning": {"summary": "auto"}
                }
            }),
            Some(ThinkingRequestV1::enabled().with_effort("medium")),
            Some(ThinkingEncodingV1::OpenaiResponses),
        );
        let body = crate::openai_responses_typed::encode_responses_request_for_profile(
            &responses,
            OpenAiResponsesProfile::Standard,
        )
        .unwrap();
        assert_eq!(body["prompt_cache_key"], "responses-key");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["reasoning"]["effort"], "medium");

        let anthropic = request(
            serde_json::json!({
                "anthropic": {
                    "system": [
                        {"type":"text","text":"policy","cache_control":{"type":"ephemeral"}}
                    ]
                }
            }),
            Some(ThinkingRequestV1::enabled().with_budget(4096)),
            Some(ThinkingEncodingV1::AnthropicMessages),
        );
        let mut anthropic = anthropic;
        anthropic.max_output_tokens = Some(1024);
        let body = crate::anthropic_typed::encode_anthropic_request(&anthropic).unwrap();
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type":"ephemeral"})
        );
        assert_eq!(body["thinking"]["budget_tokens"], 4096);

        let gemini = request(
            serde_json::json!({"gemini": {"cachedContent": "caches/abc"}}),
            Some(ThinkingRequestV1::enabled().with_budget(8192)),
            Some(ThinkingEncodingV1::GeminiGenerateContent),
        );
        let body = crate::gemini_typed::encode_gemini_request(&gemini).unwrap();
        assert_eq!(body["cachedContent"], "caches/abc");
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            8192
        );
    }
}
