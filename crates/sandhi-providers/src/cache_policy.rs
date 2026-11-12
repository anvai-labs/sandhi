//! Compile declared cache intent once, then apply the same bounded mutations on
//! typed and raw native bodies. No usage counters or inference replay live here.
use crate::{OpenAiResponsesProfile, ProviderError, ProviderFamily, ProviderHandle};
use sandhi_core::{
    cache_digest, CacheCapabilitiesV1, CacheCapabilityProfileV1 as Profile, CacheIntentV1,
    CacheModeV1 as Mode, CacheMutationV1 as Mutation, CacheNamespaceV1 as Namespace, ChatRequestV1,
    CompiledCachePlanV1 as Plan, EndpointFamilyV1 as Family,
};
use serde_json::{json, Value};

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::InvalidRequest(message.into())
}

impl ProviderHandle {
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
            self.slug(),
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
            return Ok(request);
        }
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
