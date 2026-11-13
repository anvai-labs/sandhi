//! Issuer proof is independent of affinity, cache identity and measured usage.
//! Only trusted HTTP decoder fields and native source output enter this layer.
use crate::{codec::IngressDialect, ingress_family, ProxyState};
use axum::{
    body::{to_bytes, Body, Bytes},
    response::Response,
};
use futures_util::StreamExt;
use sandhi_core::{cache_digest, ChatRequestV1, EndpointFamilyV1 as Family, VirtualKey};
use sandhi_providers::ProviderHandle;
use sandhi_store::references::{Reference, ReferenceStore, MAX_BATCH, MAX_TTL_SECS};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Copy)]
pub(crate) struct NativeEmission(pub bool);

const LIMIT: usize = 8 * 1024 * 1024;
const RESPONSE: u8 = 1;
const ITEM: u8 = 2;
const ENCRYPTED: u8 = sandhi_store::references::ENCRYPTED_REASONING;
const FILE: u8 = 4;
const CACHE: u8 = 5;
const SIGNATURE: u8 = sandhi_store::references::THINKING_SIGNATURE;
const REDACTED: u8 = sandhi_store::references::REDACTED_THINKING;
const CONTAINER: u8 = 8;
const CONVERSATION: u8 = 9;
const PROMPT: u8 = 10;
const VECTOR: u8 = 11;
const AUDIO: u8 = 12;
const SKILL: u8 = 13;
const CONNECTOR: u8 = 14;
const MESSAGE:&str="native reference is unverified for this caller and backend; materialize the input or use state issued to this exact scope";
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn failure() -> &'static str {
    MESSAGE
}
#[derive(Clone)]
pub(crate) struct Context {
    store: Arc<ReferenceStore>,
    slots: Arc<tokio::sync::Semaphore>,
    binding: String,
    owner: String,
    expiry: i64,
    family: Family,
    input_references: bool,
    streaming: bool,
}
#[derive(Default)]
struct References {
    values: Vec<Reference>,
    seen: HashSet<Reference>,
}
impl References {
    fn add(&mut self, kind: u8, value: &Value) -> Result<(), &'static str> {
        if value.is_null() {
            return Ok(());
        }
        let text = value
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(failure)?;
        if text.len() > LIMIT {
            return Err(failure());
        }
        let r = Reference {
            kind,
            digest: format!("{:x}", Sha256::digest(text.as_bytes())),
        };
        if !self.seen.contains(&r) {
            if self.values.len() >= MAX_BATCH {
                return Err(failure());
            }
            self.seen.insert(r.clone());
            self.values.push(r);
        }
        Ok(())
    }
    fn list(&mut self, kind: u8, value: &Value) -> Result<(), &'static str> {
        if value.is_null() {
            return Ok(());
        }
        for entry in value.as_array().ok_or_else(failure)? {
            self.add(kind, entry)?;
        }
        Ok(())
    }
    fn field(&mut self, kind: u8, value: &Value, key: &str) -> Result<(), &'static str> {
        if let Some(v) = value.get(key) {
            self.add(kind, v)?
        }
        Ok(())
    }
}
fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
/// Known structured content only. JSON-looking prose and function arguments are not traversed.
fn content(value: &Value, refs: &mut References) -> Result<(), &'static str> {
    if value.is_array() {
        for v in array(value) {
            content(v, refs)?
        }
        return Ok(());
    }
    if !value.is_object() {
        return Ok(());
    }
    refs.field(FILE, value, "file_id")?;
    if let Some(file) = value.get("file") {
        refs.field(FILE, file, "file_id")?
    }
    for key in ["fileData", "file_data"] {
        if let Some(file) = value.get(key) {
            refs.field(FILE, file, "fileUri")?;
            refs.field(FILE, file, "file_uri")?;
        }
    }
    if let Some(source) = value.get("source") {
        refs.field(FILE, source, "file_id")?
    }
    refs.field(SIGNATURE, value, "thoughtSignature")?;
    refs.field(SIGNATURE, value, "thought_signature")?;
    match value.get("type").and_then(Value::as_str) {
        Some("thinking") => {
            if value.get("signature").and_then(Value::as_str) != Some("") {
                refs.field(SIGNATURE, value, "signature")?
            }
        }
        Some("redacted_thinking") => refs.field(REDACTED, value, "data")?,
        Some("reasoning") => refs.field(ENCRYPTED, value, "encrypted_content")?,
        _ => {}
    }
    for key in [
        "content",
        "parts",
        "thinking_blocks",
        "functionResponse",
        "function_response",
    ] {
        if let Some(child) = value.get(key) {
            content(child, refs)?
        }
    }
    Ok(())
}
fn id(refs: &mut References, kind: u8, value: &Value) -> Result<(), &'static str> {
    if value.is_object() {
        refs.add(kind, value.get("id").ok_or_else(failure)?)
    } else {
        refs.add(kind, value)
    }
}
fn request_references(body: &Value, family: Family) -> Result<Vec<Reference>, &'static str> {
    let mut refs = References::default();
    match family {
        Family::OpenaiResponses => {
            refs.field(RESPONSE, body, "previous_response_id")?;
            for (key, kind) in [("conversation", CONVERSATION), ("prompt", PROMPT)] {
                if let Some(v) = body.get(key).filter(|v| !v.is_null()) {
                    id(&mut refs, kind, v)?
                }
            }
            for item in array(&body["input"]) {
                // Even a materialized native item carrying a server ID requires its issuer proof.
                refs.field(ITEM, item, "id")?;
                if item.get("type").and_then(Value::as_str) == Some("item_reference")
                    && item.get("id").is_none()
                {
                    return Err(failure());
                }
                content(item, &mut refs)?;
                if item.get("output").is_some() {
                    content(&item["output"], &mut refs)?
                }
            }
            for tool in array(&body["tools"]) {
                match tool.get("type").and_then(Value::as_str) {
                    Some("file_search") => {
                        refs.list(VECTOR, &tool["vector_store_ids"])?;
                    }
                    Some("code_interpreter") => {
                        if let Some(v) = tool.get("container").filter(|v| !v.is_null()) {
                            if v.is_object()
                                && v.get("type").and_then(Value::as_str) == Some("auto")
                            {
                                refs.list(FILE, &v["file_ids"])?;
                            } else if v.as_str() != Some("auto") {
                                id(&mut refs, CONTAINER, v)?
                            }
                        }
                    }
                    Some("image_generation") => content(&tool["input_image_mask"], &mut refs)?,
                    Some("mcp") => refs.field(CONNECTOR, tool, "connector_id")?,
                    _ => {}
                }
            }
        }
        Family::AnthropicMessages => {
            if let Some(v) = body.get("container").filter(|v| !v.is_null()) {
                if v.is_object() {
                    refs.field(CONTAINER, v, "id")?;
                    for skill in array(&v["skills"]) {
                        if skill.get("type").and_then(Value::as_str) != Some("anthropic") {
                            refs.field(SKILL, skill, "skill_id")?;
                        }
                    }
                } else {
                    id(&mut refs, CONTAINER, v)?;
                }
            }
            content(&body["messages"], &mut refs)?;
            content(&body["system"], &mut refs)?;
        }
        Family::GeminiGenerateContent => {
            refs.field(CACHE, body, "cachedContent")?;
            refs.field(CACHE, body, "cached_content")?;
            content(&body["contents"], &mut refs)?;
            content(&body["systemInstruction"], &mut refs)?;
            content(&body["system_instruction"], &mut refs)?;
            for tool in array(&body["tools"]) {
                for name in ["fileSearch", "file_search"] {
                    if let Some(search) = tool.get(name) {
                        for key in ["fileSearchStoreNames", "file_search_store_names"] {
                            refs.list(VECTOR, &search[key])?;
                        }
                    }
                }
                if let Some(retrieval) = tool.get("retrieval") {
                    for name in ["vertexAiSearch", "vertex_ai_search"] {
                        if let Some(search) = retrieval.get(name) {
                            for key in ["datastore", "engine"] {
                                refs.field(VECTOR, search, key)?;
                            }
                            for key in ["dataStoreSpecs", "data_store_specs"] {
                                for spec in array(&search[key]) {
                                    refs.field(VECTOR, spec, "dataStore")?;
                                    refs.field(VECTOR, spec, "data_store")?;
                                }
                            }
                        }
                    }
                    for name in ["vertexRagStore", "vertex_rag_store"] {
                        if let Some(rag) = retrieval.get(name) {
                            for key in ["ragCorpora", "rag_corpora"] {
                                refs.list(VECTOR, &rag[key])?;
                            }
                            for key in ["ragResources", "rag_resources"] {
                                for resource in array(&rag[key]) {
                                    refs.field(VECTOR, resource, "ragCorpus")?;
                                    refs.field(VECTOR, resource, "rag_corpus")?;
                                    for key in ["ragFileIds", "rag_file_ids"] {
                                        refs.list(FILE, &resource[key])?;
                                    }
                                }
                            }
                        }
                    }
                    // A configured external retriever is not an inferred grant from a VK.
                    if retrieval
                        .get("externalApi")
                        .or_else(|| retrieval.get("external_api"))
                        .is_some_and(|v| !v.is_null())
                    {
                        return Err(failure());
                    }
                }
            }
        }
        Family::OpenaiChatCompletions => {
            for message in array(&body["messages"]) {
                content(message, &mut refs)?;
                refs.field(AUDIO, &message["audio"], "id")?;
                if let Some(v) = message.get("reasoning").filter(|v| v.is_object()) {
                    refs.field(ENCRYPTED, v, "encrypted_content")?
                }
            }
        }
        _ => {}
    }
    Ok(refs.values)
}
#[derive(Debug)]
pub(crate) enum PreparationError {
    Ownership,
    Fidelity(String),
    Credential,
}
impl From<&'static str> for PreparationError {
    fn from(_: &'static str) -> Self {
        Self::Ownership
    }
}
impl PreparationError {
    pub(crate) fn into_response(self, dialect: IngressDialect) -> Response {
        match self {
            Self::Ownership => {
                crate::ingress_error(dialect, axum::http::StatusCode::FORBIDDEN, MESSAGE)
            }
            Self::Fidelity(message) => {
                crate::ingress_error(dialect, axum::http::StatusCode::BAD_REQUEST, &message)
            }
            Self::Credential => crate::provider_error(
                &sandhi_providers::ProviderError::Auth,
                dialect,
                "sandhi",
                false,
            ),
        }
    }
}

/// Runs before a pin is confirmed and before any lease or provider dispatch.
pub(crate) async fn prepare_effective(
    state: &ProxyState,
    vk: &VirtualKey,
    provider: &ProviderHandle,
    request: &ChatRequestV1,
    dialect: IngressDialect,
    effective: &Value,
) -> Result<Option<Context>, PreparationError> {
    if !provider.reference_credential_is_current() {
        return Err(PreparationError::Credential);
    }
    // These native endpoints carry materialized text/token/vector data, not
    // generation references. Preserve their existing response bounds/streaming.
    if matches!(
        dialect,
        IngressDialect::Embeddings | IngressDialect::Completions
    ) {
        return Ok(None);
    }
    let family = provider.family().endpoint_family();
    if ingress_family(dialect).endpoint_family() != family {
        // A faithful materialized projection does not send native item metadata
        // to the other protocol. Native opaque/resource fields still fail the
        // decoder-owned fidelity fence; they never gain authority by translation.
        request
            .native_input_for(provider.family().endpoint_family())
            .map_err(PreparationError::Fidelity)?;
    }
    let refs = request_references(effective, family)?;
    let authority = crate::settlement::buffered::reference_authority(state);
    let credential = provider.routing_credential_fingerprint();
    // Escape-hatch handles lack a qualified transport/account identity and cannot issue proof.
    if authority.is_none() || credential.is_none() {
        return if refs.is_empty() {
            Ok(None)
        } else {
            Err(PreparationError::Ownership)
        };
    }
    let (store, slots) = authority.expect("checked authority");
    let mut ctx = frozen_context(store, slots, vk, provider, request, family, now())?;
    ctx.input_references = !refs.is_empty();
    if !refs.is_empty() {
        let c = ctx.clone();
        let permit = c.slots.clone().try_acquire_owned().map_err(|_| failure())?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            c.store.authorize(&c.binding, &c.owner, &refs)
        })
        .await
        .map_err(|_| failure())?
        .map_err(|_| failure())?;
    }
    if ctx.expiry <= now() {
        return Err(PreparationError::Ownership);
    }
    if !provider.reference_credential_is_current() {
        return Err(PreparationError::Credential);
    }
    Ok(Some(ctx))
}
#[cfg(test)]
async fn prepare(
    state: &ProxyState,
    vk: &VirtualKey,
    provider: &ProviderHandle,
    request: &ChatRequestV1,
    dialect: IngressDialect,
) -> Result<Option<Context>, PreparationError> {
    if matches!(
        dialect,
        IngressDialect::Embeddings | IngressDialect::Completions
    ) {
        return prepare_effective(state, vk, provider, request, dialect, &Value::Null).await;
    }
    let effective = provider
        .encode_execution_request(request)
        .map_err(|error| match error {
            sandhi_providers::ProviderError::Auth => PreparationError::Credential,
            other => PreparationError::Fidelity(other.to_string()),
        })?;
    prepare_effective(state, vk, provider, request, dialect, &effective).await
}

fn frozen_context(
    store: Arc<ReferenceStore>,
    slots: Arc<tokio::sync::Semaphore>,
    vk: &VirtualKey,
    provider: &ProviderHandle,
    request: &ChatRequestV1,
    family: Family,
    clock: i64,
) -> Result<Context, &'static str> {
    let credential = provider
        .routing_credential_fingerprint()
        .ok_or_else(failure)?;
    let owner = cache_digest(&(
        "native-reference-owner-v1",
        &vk.id,
        &vk.subject_id,
        &vk.group_id,
        &vk.upstream_ref,
    ))
    .map_err(|_| failure())?;
    let binding = provider.backend_binding().map(|b| {
        (
            &b.id,
            b.revision,
            &b.credential_ref,
            (!provider.has_account_fenced_credential()).then_some(b.credential_generation),
            b.endpoint_family,
            &b.endpoint_profile,
        )
    });
    let binding = cache_digest(&(
        "native-reference-binding-v1",
        &vk.upstream_ref,
        binding,
        credential,
        provider.family().endpoint_family(),
        provider
            .source_usage_convention()
            .ok()
            .map(|v| v.wire_value()),
        &request.model,
    ))
    .map_err(|_| failure())?;
    let mut expiry = clock.saturating_add(MAX_TTL_SECS);
    if let Some(exp) = &vk.expires_at {
        expiry = expiry.min(
            time::OffsetDateTime::parse(exp, &time::format_description::well_known::Rfc3339)
                .map_err(|_| failure())?
                .unix_timestamp(),
        );
    }
    if !provider.has_account_fenced_credential() {
        if let Some(exp) = provider.reference_credential_expiry() {
            expiry = expiry.min(i64::try_from(exp).map_err(|_| failure())?);
        }
    }
    if expiry <= clock {
        return Err(failure());
    }
    Ok(Context {
        store,
        slots,
        binding,
        owner,
        expiry,
        family,
        input_references: false,
        streaming: false,
    })
}

impl Context {
    pub(crate) fn with_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }
    pub(crate) fn has_input_references(&self) -> bool {
        self.input_references
    }
    async fn issue(&self, refs: Vec<Reference>) -> Result<(), &'static str> {
        if refs.is_empty() {
            return Ok(());
        }
        let c = self.clone();
        let permit = c.slots.clone().try_acquire_owned().map_err(|_| failure())?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            c.store.issue(&c.binding, &c.owner, &refs, c.expiry)
        })
        .await
        .map_err(|_| failure())?
        .map_err(|_| failure())
    }
}

fn response_refs(body: &Value, family: Family) -> Result<Vec<Reference>, &'static str> {
    let mut refs = References::default();
    match family {
        Family::OpenaiResponses => {
            let response = body
                .get("response")
                .or_else(|| {
                    (body.get("object").and_then(Value::as_str) == Some("response")
                        || body.get("output").is_some_and(Value::is_array))
                    .then_some(body)
                })
                .unwrap_or(&Value::Null);
            refs.field(RESPONSE, response, "id")?;
            if let Some(v) = response.get("conversation").filter(|v| !v.is_null()) {
                id(&mut refs, CONVERSATION, v)?
            }
            for item in array(&response["output"]) {
                refs.field(ITEM, item, "id")?;
                refs.field(CONTAINER, item, "container_id")?;
                content(item, &mut refs)?
            }
            if let Some(item) = body.get("item") {
                refs.field(ITEM, item, "id")?;
                refs.field(CONTAINER, item, "container_id")?;
                content(item, &mut refs)?
            }
            refs.field(ITEM, body, "item_id")?;
        }
        Family::AnthropicMessages => {
            content(&body["content"], &mut refs)?;
            content(&body["content_block"], &mut refs)?;
            content(&body["message"], &mut refs)?;
            if let Some(v) = body.pointer("/message/container").filter(|v| !v.is_null()) {
                id(&mut refs, CONTAINER, v)?;
            }
            if let Some(v) = body.get("container").filter(|v| !v.is_null()) {
                id(&mut refs, CONTAINER, v)?
            }
        }
        Family::GeminiGenerateContent => {
            for candidate in array(&body["candidates"]) {
                content(&candidate["content"], &mut refs)?
            }
        }
        Family::OpenaiChatCompletions => {
            for choice in array(&body["choices"]) {
                let message = choice
                    .get("message")
                    .or_else(|| choice.get("delta"))
                    .unwrap_or(&Value::Null);
                content(message, &mut refs)?;
                refs.field(AUDIO, &message["audio"], "id")?;
                if let Some(v) = message.get("reasoning").filter(|v| v.is_object()) {
                    refs.field(ENCRYPTED, v, "encrypted_content")?
                }
            }
        }
        _ => {}
    }
    Ok(refs.values)
}

/// Bounded byte-preserving complete SSE event framing, including split CRLF/CR and multiline data.
#[derive(Default)]
struct Frames {
    wire: Vec<u8>,
    line_start: usize,
    cr: bool,
}
impl Frames {
    fn line(&mut self, end: usize) -> Option<Bytes> {
        let empty = end == self.line_start;
        self.line_start = self.wire.len();
        if empty {
            self.line_start = 0;
            Some(Bytes::from(std::mem::take(&mut self.wire)))
        } else {
            None
        }
    }
    fn byte(&mut self, b: u8) -> Result<Vec<Bytes>, &'static str> {
        let mut complete = Vec::new();
        if self.cr {
            self.cr = false;
            if b == b'\n' {
                if self.wire.is_empty() {
                    // The CR already completed and authorized the event. Keep
                    // its optional LF byte without delaying a bare-CR event.
                    complete.push(Bytes::from_static(b"\n"));
                    return Ok(complete);
                }
                if self.wire.len() >= LIMIT {
                    return Err(failure());
                }
                self.wire.push(b);
                self.line_start = self.wire.len();
                return Ok(complete);
            }
        }
        if self.wire.len() >= LIMIT {
            return Err(failure());
        }
        self.wire.push(b);
        match b {
            b'\r' => {
                self.cr = true;
                if let Some(frame) = self.line(self.wire.len() - 1) {
                    complete.push(frame)
                }
            }
            b'\n' => {
                if let Some(frame) = self.line(self.wire.len() - 1) {
                    complete.push(frame)
                }
            }
            _ => {}
        }
        Ok(complete)
    }
    fn end(&mut self) -> Option<Bytes> {
        self.cr = false;
        None
    }
}
fn frame_json(wire: &[u8]) -> Result<Option<Value>, &'static str> {
    let text = std::str::from_utf8(wire)
        .map_err(|_| failure())?
        .trim_start_matches('\u{feff}');
    let mut data = String::new();
    let mut has = false;
    for line in text.split(['\r', '\n']) {
        if let Some(part) = line
            .strip_prefix("data:")
            .or_else(|| (line == "data").then_some(""))
        {
            if has {
                data.push('\n')
            }
            data.push_str(part.strip_prefix(' ').unwrap_or(part));
            has = true;
        }
    }
    if !has || data.trim() == "[DONE]" {
        return Ok(None);
    }
    sandhi_core::policy::strict_json(data.as_bytes())
        .map(Some)
        .map_err(|_| failure())
}

/// Captures fragmented Anthropic signatures without exposing their first fragment before proof.
#[derive(Default)]
struct Signature {
    index: Option<u64>,
    text: String,
    held: Vec<Bytes>,
    bytes: usize,
}
impl Signature {
    fn observe(
        &mut self,
        body: &Value,
        frame: Bytes,
    ) -> Result<(Vec<Reference>, Vec<Bytes>), &'static str> {
        let signature = body.get("type").and_then(Value::as_str) == Some("content_block_delta")
            && body.pointer("/delta/type").and_then(Value::as_str) == Some("signature_delta");
        if signature {
            let index = body
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(failure)?;
            if self.index.is_some_and(|old| old != index) {
                return Err(failure());
            }
            let text = body
                .pointer("/delta/signature")
                .and_then(Value::as_str)
                .ok_or_else(failure)?;
            if self
                .text
                .len()
                .saturating_add(text.len())
                .saturating_add(self.bytes)
                .saturating_add(frame.len())
                .saturating_add(128)
                > LIMIT
            {
                return Err(failure());
            }
            self.index = Some(index);
            self.text.push_str(text);
        }
        if self.index.is_none() {
            return Ok((Vec::new(), vec![frame]));
        }
        if self
            .bytes
            .saturating_add(frame.len())
            .saturating_add(128)
            .saturating_add(self.text.len())
            > LIMIT
        {
            return Err(failure());
        }
        self.bytes += frame.len().saturating_add(128);
        self.held.push(frame);
        if body.get("type").and_then(Value::as_str) == Some("content_block_stop") {
            if body.get("index").and_then(Value::as_u64) != self.index {
                return Err(failure());
            }
            let mut refs = References::default();
            refs.add(SIGNATURE, &Value::String(std::mem::take(&mut self.text)))?;
            self.index = None;
            self.bytes = 0;
            return Ok((refs.values, std::mem::take(&mut self.held)));
        }
        Ok((Vec::new(), Vec::new()))
    }
}

struct Delivery {
    context: Context,
    signatures: Signature,
    issued: HashSet<Reference>,
}
impl Delivery {
    async fn record(&mut self, refs: Vec<Reference>) -> Result<(), &'static str> {
        let fresh = refs
            .into_iter()
            .filter(|r| !self.issued.contains(r))
            .collect::<Vec<_>>();
        if self.issued.len().saturating_add(fresh.len()) > MAX_BATCH {
            return Err(failure());
        }
        self.context.issue(fresh.clone()).await?;
        self.issued.extend(fresh);
        Ok(())
    }
    async fn frame(&mut self, frame: Bytes) -> Result<Vec<Bytes>, &'static str> {
        let value = frame_json(&frame)?;
        if let Some(value) = value.as_ref() {
            self.record(response_refs(value, self.context.family)?)
                .await?
        }
        if self.context.family != Family::AnthropicMessages {
            return Ok(vec![frame]);
        }
        let (refs, ready) = if let Some(value) = value.as_ref() {
            self.signatures.observe(value, frame)?
        } else if self.signatures.index.is_some() {
            if self
                .signatures
                .bytes
                .saturating_add(frame.len())
                .saturating_add(128)
                .saturating_add(self.signatures.text.len())
                > LIMIT
            {
                return Err(failure());
            }
            self.signatures.bytes += frame.len().saturating_add(128);
            self.signatures.held.push(frame);
            (Vec::new(), Vec::new())
        } else {
            (Vec::new(), vec![frame])
        };
        self.record(refs).await?;
        Ok(ready)
    }
}

pub(crate) async fn protect(response: Response, context: Option<Context>) -> Response {
    let Some(context) = context else {
        return response;
    };
    if !response.status().is_success()
        || !response
            .extensions()
            .get::<NativeEmission>()
            .is_some_and(|emission| emission.0)
    {
        return response;
    }
    // Delivery mode is fixed by the server plan, independently of origin headers.
    let is_stream = context.streaming;
    let (mut parts, body) = response.into_parts();
    if !is_stream {
        let bytes = match to_bytes(body, LIMIT).await {
            Ok(v) => v,
            Err(_) => return proof_error(context.family, &parts.headers),
        };
        let result = match sandhi_core::policy::strict_json(&bytes) {
            Ok(value) => match response_refs(&value, context.family) {
                Ok(refs) => context.issue(refs).await,
                Err(e) => Err(e),
            },
            Err(_) => Err(failure()),
        };
        if result.is_err() {
            return proof_error(context.family, &parts.headers);
        }
        return Response::from_parts(parts, Body::from(bytes));
    }
    parts.headers.remove("content-length");
    parts.headers.insert(
        "x-should-retry",
        axum::http::HeaderValue::from_static("false"),
    );
    let stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>,
    > = Box::pin(async_stream::try_stream! {
     let mut source=body.into_data_stream();let mut frames=Frames::default();let mut first=None;let mut source_error=None;
     let mut delivery=Delivery{context,signatures:Signature::default(),issued:HashSet::new()};
     while let Some(chunk)=source.next().await {
      let bytes=match chunk {Ok(b)=>b,Err(error)=>{source_error=Some(error);break}};
      if first.is_some() {continue}
      for &byte in bytes.as_ref() {
       let ready=match frames.byte(byte) {Ok(v)=>v,Err(e)=>{first=Some(e);break}};
       for frame in ready {
        match delivery.frame(frame).await {
         Ok(ready)=>for frame in ready {yield frame},
         Err(e)=>{first=Some(e);break}
        }
       }
       if first.is_some() {break}
      }
     }
     if first.is_none() && source_error.is_none() {
      if let Some(frame)=frames.end() {
       for frame in delivery.frame(frame).await.map_err(std::io::Error::other)? {yield frame}
      }
      if !frames.wire.is_empty() {
       let tail=Bytes::from(std::mem::take(&mut frames.wire));
       if std::str::from_utf8(&tail).is_ok_and(|text| !text.split(['\r','\n']).any(|line|line.starts_with("data:")||line=="data")) && delivery.signatures.index.is_none() {yield tail}
       else {first=Some(failure())}
      }
      if delivery.signatures.index.is_some() {first=Some(failure())}
     }
     if let Some(message)=first {Err(std::io::Error::other(message))?}
     if let Some(error)=source_error {Err(std::io::Error::other(error))?}
    });
    Response::from_parts(parts, Body::from_stream(stream))
}
fn proof_error(family: Family, headers: &axum::http::HeaderMap) -> Response {
    let error = sandhi_providers::ProviderError::OutputProjection(
        "native issuer proof could not be persisted; do not retry".into(),
    );
    let mut response = crate::provider_error(&error, family_dialect(family), "sandhi", false);
    if let Some(id) = headers.get("x-sandhi-request-id") {
        response
            .headers_mut()
            .insert("x-sandhi-request-id", id.clone());
    }
    response
}
fn family_dialect(f: Family) -> IngressDialect {
    match f {
        Family::OpenaiResponses => IngressDialect::Responses,
        Family::AnthropicMessages => IngressDialect::Anthropic,
        Family::GeminiGenerateContent => IngressDialect::Gemini,
        _ => IngressDialect::OpenAi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn references_use_known_structures_not_caller_text_or_tool_arguments() {
        let cases = [
            (
                Family::OpenaiResponses,
                json!({"previous_response_id":"resp_A","input":[{"type":"item_reference","id":"msg_A"},{"type":"reasoning","encrypted_content":"opaque_A"}]}),
                3,
            ),
            (
                Family::OpenaiResponses,
                json!({"input":[{"type":"function_call_output","output":[{"type":"input_file","file_id":"file_A"}]}]}),
                1,
            ),
            (
                Family::GeminiGenerateContent,
                json!({"cachedContent":"cachedContents/A","contents":[{"parts":[{"fileData":{"fileUri":"files/A"}},{"thoughtSignature":"signed_A","text":"x"}]}]}),
                3,
            ),
            (
                Family::AnthropicMessages,
                json!({"container":"container_A","messages":[{"content":[{"type":"thinking","thinking":"x","signature":"signed_A"},{"type":"redacted_thinking","data":"opaque_A"},{"type":"document","source":{"type":"file","file_id":"file_A"}}]}]}),
                4,
            ),
            (
                Family::OpenaiChatCompletions,
                json!({"messages":[{"audio":{"id":"audio_A"},"content":[{"type":"file","file":{"file_id":"file_A"}}]}]}),
                2,
            ),
        ];
        for (f, body, count) in cases {
            assert_eq!(request_references(&body, f).unwrap().len(), count)
        }
        assert!(request_references(&json!({"input":[{"role":"user","content":"{\"file_id\":\"not-a-reference\"}"},{"type":"function_call","call_id":"call_1","name":"local","arguments":"{\"previous_response_id\":\"not-native\"}"}],"tools":[{"type":"function","name":"local","parameters":{"type":"object","properties":{"file_id":{"type":"string"}}}}]}),Family::OpenaiResponses).unwrap().is_empty());
    }
    #[test]
    fn bare_cr_event_is_complete_before_another_byte_or_transport_eof() {
        let wire = b"data: {\"object\":\"response\",\"id\":\"resp_A\",\"output\":[]}\r\r";
        let mut frames = Frames::default();
        let mut emitted = Vec::new();
        for &byte in wire {
            emitted.extend(frames.byte(byte).unwrap());
        }
        assert_eq!(
            emitted.concat(),
            wire,
            "a complete CR-delimited event must not wait for HTTP EOF"
        );
        assert!(frames.wire.is_empty());
    }
    #[test]
    fn complete_sse_frames_are_byte_preserved_for_all_fragmentation_and_newlines() {
        for newline in ["\n", "\r\n", "\r"] {
            let wire="event: response.created\ndata: {\"type\":\"response.created\",\ndata: \"response\":{\"id\":\"resp_A\",\"object\":\"response\"}}\n\n: keepalive\n\n".replace('\n',newline);
            for split in 0..=wire.len() {
                let mut frames = Frames::default();
                let mut output = Vec::new();
                for part in [&wire.as_bytes()[..split], &wire.as_bytes()[split..]] {
                    for &byte in part {
                        output.extend(frames.byte(byte).unwrap())
                    }
                }
                if let Some(frame) = frames.end() {
                    output.push(frame)
                }
                assert!(frames.wire.is_empty());
                assert_eq!(output.concat(), wire.as_bytes());
                assert_eq!(
                    response_refs(
                        &frame_json(&output[0]).unwrap().unwrap(),
                        Family::OpenaiResponses
                    )
                    .unwrap()
                    .len(),
                    1
                );
            }
        }
        let mut frames = Frames::default();
        for b in b"data: {\"id\":\"uncommitted\"}" {
            assert!(frames.byte(*b).unwrap().is_empty())
        }
        assert!(frames.end().is_none());
    }
    #[tokio::test]
    async fn fragmented_signature_proof_precedes_its_first_exposed_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(ReferenceStore::open(dir.path().join("refs.db").to_str().unwrap()).unwrap());
        let ctx = Context {
            store: store.clone(),
            slots: Arc::new(tokio::sync::Semaphore::new(1)),
            binding: "a".repeat(64),
            owner: "b".repeat(64),
            expiry: now() + 100,
            family: Family::AnthropicMessages,
            input_references: false,
            streaming: true,
        };
        let mut delivery = Delivery {
            context: ctx.clone(),
            signatures: Signature::default(),
            issued: HashSet::new(),
        };
        for fragment in ["signed_", "A"] {
            let frame = Bytes::from(format!(
                "data: {}\n\n",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":fragment}})
            ));
            assert!(delivery.frame(frame).await.unwrap().is_empty());
        }
        let mut refs = References::default();
        refs.add(SIGNATURE, &json!("signed_A")).unwrap();
        assert!(store
            .authorize(&ctx.binding, &ctx.owner, &refs.values)
            .is_err());
        let end = Bytes::from_static(b"data: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
        assert_eq!(delivery.frame(end).await.unwrap().len(), 3);
        store
            .authorize(&ctx.binding, &ctx.owner, &refs.values)
            .unwrap();
    }
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    use axum::response::IntoResponse;
    use sandhi_core::{InMemorySink, KeyStore, RequestMetadataV1};
    use sandhi_providers::{BackendBinding, ProviderRuntime};
    use serde_json::json;
    use std::collections::HashMap;
    fn provider(secret: &str, url: &str, account: Option<&str>) -> ProviderHandle {
        let runtime = ProviderRuntime::new();
        let mut headers = axum::http::HeaderMap::new();
        if let Some(account) = account {
            headers.insert("chatgpt-account-id", account.parse().unwrap());
        }
        let p = runtime.openai_responses("vendor", url, secret, headers, Some(0), None, None, None);
        p.with_backend_binding(BackendBinding {
            id: "backend".into(),
            revision: 1,
            provider: "vendor".into(),
            credential_ref: "vendor:credential".into(),
            credential_generation: 1,
            endpoint_family: Family::OpenaiResponses,
            endpoint_profile: "standard".into(),
            models: vec![serde_json::from_value(json!({"id":"model"})).unwrap()],
            cache_policy: None,
        })
    }
    fn request(reference: bool, model: &str) -> ChatRequestV1 {
        let mut body = json!({"model":model,"input":"materialized input"});
        if reference {
            body["previous_response_id"] = json!("resp_owner");
        }
        crate::codec::decode_request(
            IngressDialect::Responses,
            body,
            RequestMetadataV1::default(),
        )
        .unwrap()
        .0
    }
    #[tokio::test]
    async fn every_frozen_binding_and_authenticated_owner_dimension_is_fenced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let state = Arc::new(ProxyState::new(
            KeyStore::new(),
            crate::ProxyLedger::durable(path.to_str().unwrap(), 1).unwrap(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        ));
        crate::initialize_execution(&state).unwrap();
        let vk = VirtualKey {
            id: "key_owner".into(),
            subject_id: Some("subject".into()),
            group_id: Some("group".into()),
            upstream_ref: "backend/backend".into(),
            ..Default::default()
        };
        let p = provider("synthetic-secret", "http://127.0.0.1:1", Some("account_A"));
        let ctx = prepare(
            &state,
            &vk,
            &p,
            &request(false, "model"),
            IngressDialect::Responses,
        )
        .await
        .unwrap()
        .unwrap();
        let mut refs = References::default();
        refs.add(RESPONSE, &json!("resp_owner")).unwrap();
        ctx.issue(refs.values).await.unwrap();
        prepare(
            &state,
            &vk,
            &p,
            &request(true, "model"),
            IngressDialect::Responses,
        )
        .await
        .unwrap();
        for field in ["id", "subject", "group", "upstream"] {
            let mut changed = vk.clone();
            match field {
                "id" => changed.id = "other-key".into(),
                "subject" => changed.subject_id = Some("other-subject".into()),
                "group" => changed.group_id = Some("other-group".into()),
                _ => changed.upstream_ref = "backend/other".into(),
            }
            assert!(
                prepare(
                    &state,
                    &changed,
                    &p,
                    &request(true, "model"),
                    IngressDialect::Responses
                )
                .await
                .is_err(),
                "{field}"
            );
        }
        for field in [
            "backend",
            "revision",
            "credential",
            "generation",
            "profile",
            "family",
        ] {
            let mut binding = p.backend_binding().unwrap().clone();
            match field {
                "backend" => binding.id = "other".into(),
                "revision" => binding.revision += 1,
                "credential" => binding.credential_ref = "vendor:other".into(),
                "generation" => binding.credential_generation += 1,
                "profile" => binding.endpoint_profile = "other".into(),
                _ => binding.endpoint_family = Family::OpenaiChatCompletions,
            }
            let changed = p.clone().with_backend_binding(binding);
            assert!(
                prepare(
                    &state,
                    &vk,
                    &changed,
                    &request(true, "model"),
                    IngressDialect::Responses
                )
                .await
                .is_err(),
                "{field}"
            );
        }
        for changed in [
            provider(
                "replacement-secret",
                "http://127.0.0.1:1",
                Some("account_A"),
            ),
            provider("synthetic-secret", "http://127.0.0.1:2", Some("account_A")),
            provider("synthetic-secret", "http://127.0.0.1:1", Some("account_B")),
        ] {
            assert!(prepare(
                &state,
                &vk,
                &changed,
                &request(true, "model"),
                IngressDialect::Responses
            )
            .await
            .is_err());
        }
        assert!(prepare(
            &state,
            &vk,
            &p,
            &request(true, "other-model"),
            IngressDialect::Responses
        )
        .await
        .is_err());
        let expired = p.clone().with_credential_expiry((now() - 1) as u64);
        assert!(prepare(
            &state,
            &vk,
            &expired,
            &request(true, "model"),
            IngressDialect::Responses
        )
        .await
        .is_err());
        let mut expired_key = vk.clone();
        expired_key.expires_at = Some("2000-01-01T00:00:00Z".into());
        assert!(prepare(
            &state,
            &expired_key,
            &p,
            &request(true, "model"),
            IngressDialect::Responses
        )
        .await
        .is_err());
        let held = crate::settlement::buffered::reference_authority(&state)
            .unwrap()
            .1
            .acquire_many_owned(4)
            .await
            .unwrap();
        assert!(prepare(
            &state,
            &vk,
            &p,
            &request(true, "model"),
            IngressDialect::Responses
        )
        .await
        .is_err());
        drop(held);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE issued_native_references SET expires_at=?1",
            [now() - 1],
        )
        .unwrap();
        assert!(prepare(
            &state,
            &vk,
            &p,
            &request(true, "model"),
            IngressDialect::Responses
        )
        .await
        .is_err());
        let unavailable = ProxyState::new(
            KeyStore::new(),
            crate::ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        );
        assert!(prepare(
            &unavailable,
            &vk,
            &p,
            &request(true, "model"),
            IngressDialect::Responses
        )
        .await
        .is_err());
    }
    #[tokio::test]
    async fn canonical_rendered_ids_are_never_treated_as_native_issuer_proofs() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(ReferenceStore::open(dir.path().join("refs.db").to_str().unwrap()).unwrap());
        let ctx = Context {
            store: store.clone(),
            slots: Arc::new(tokio::sync::Semaphore::new(1)),
            binding: "a".repeat(64),
            owner: "b".repeat(64),
            expiry: now() + 100,
            family: Family::OpenaiResponses,
            input_references: false,
            streaming: false,
        };
        let response =
            axum::Json(json!({"object":"response","id":"rendered-id","output":[]})).into_response();
        let _ = protect(response, Some(ctx.clone())).await;
        let mut refs = References::default();
        refs.add(RESPONSE, &json!("rendered-id")).unwrap();
        assert!(store
            .authorize(&ctx.binding, &ctx.owner, &refs.values)
            .is_err());
    }
}

#[cfg(test)]
mod delivery_failure_tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test]
    async fn prepared_delivery_mode_controls_framing_independently_of_content_type() {
        for streaming in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(
                ReferenceStore::open(dir.path().join("refs.db").to_str().unwrap()).unwrap(),
            );
            let context = Context {
                store: store.clone(),
                slots: Arc::new(tokio::sync::Semaphore::new(1)),
                binding: "a".repeat(64),
                owner: "b".repeat(64),
                expiry: now() + 100,
                family: Family::OpenaiResponses,
                input_references: false,
                streaming,
            };
            let source = json!({"object":"response","id":"resp_issued","output":[]});
            let wire = if streaming {
                format!("data: {source}\n\n")
            } else {
                source.to_string()
            };
            let mut response = Response::new(Body::from(wire.clone()));
            response.extensions_mut().insert(NativeEmission(true));
            response.headers_mut().insert(
                "content-type",
                HeaderValue::from_static(if streaming {
                    "application/json"
                } else {
                    "text/event-stream"
                }),
            );
            let preserved = to_bytes(
                protect(response, Some(context.clone())).await.into_body(),
                LIMIT,
            )
            .await
            .unwrap();
            assert_eq!(preserved.as_ref(), wire.as_bytes());
            let mut refs = References::default();
            refs.add(RESPONSE, &json!("resp_issued")).unwrap();
            store
                .authorize(&context.binding, &context.owner, &refs.values)
                .unwrap();
        }
    }
    #[tokio::test]
    async fn first_registry_refusal_survives_source_failure_and_the_same_source_is_drained() {
        for blocked in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("refs.db");
            let store = Arc::new(ReferenceStore::open(path.to_str().unwrap()).unwrap());
            if blocked {
                rusqlite::Connection::open(&path).unwrap().execute_batch("CREATE TRIGGER fail_issue BEFORE INSERT ON issued_native_references BEGIN SELECT RAISE(ABORT,'synthetic proof failure'); END").unwrap();
            }
            let context = Context {
                store,
                slots: Arc::new(tokio::sync::Semaphore::new(1)),
                binding: "a".repeat(64),
                owner: "b".repeat(64),
                expiry: now() + 100,
                family: Family::OpenaiResponses,
                input_references: false,
                streaming: true,
            };
            let polls = Arc::new(AtomicUsize::new(0));
            let count = polls.clone();
            let stream=futures_util::stream::iter(vec![Ok(Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_owner\"}}\n\n")),Err(std::io::Error::other("later_source_timeout"))]).map(move|item|{count.fetch_add(1,Ordering::SeqCst);item});
            let mut response = Response::new(Body::from_stream(stream));
            response.extensions_mut().insert(NativeEmission(true));
            response.headers_mut().insert(
                "content-type",
                HeaderValue::from_static("text/event-stream"),
            );
            let error = to_bytes(protect(response, Some(context)).await.into_body(), LIMIT)
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(polls.load(Ordering::SeqCst), 2);
            if blocked {
                assert!(error.contains("unverified"), "{error}");
                assert!(!error.contains("later_source_timeout"));
            } else {
                assert!(error.contains("later_source_timeout"), "{error}");
            }
        }
    }
    #[test]
    fn retained_fragments_and_cardinality_refuse_before_mutation() {
        let mut refs = References::default();
        for n in 0..MAX_BATCH {
            refs.add(ITEM, &json!(format!("item_{n}"))).unwrap();
        }
        assert!(refs.add(ITEM, &json!("overflow")).is_err());
        assert_eq!(refs.values.len(), MAX_BATCH);
        let mut frames = Frames {
            wire: vec![b'x'; LIMIT],
            line_start: 0,
            cr: false,
        };
        assert!(frames.byte(b'x').is_err());
        assert_eq!(frames.wire.len(), LIMIT);
        let mut signature = Signature {
            index: Some(0),
            text: "x".into(),
            held: Vec::new(),
            bytes: LIMIT - 128,
        };
        assert!(signature
            .observe(&json!({"type":"ping"}), Bytes::from_static(b"\n\n"))
            .is_err());
        assert!(signature.held.is_empty());
        assert_eq!(signature.text, "x");
        assert_eq!(signature.bytes, LIMIT - 128);
        let mut signature = Signature::default();
        let body = json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x".repeat(7*1024*1024)}});
        assert!(signature
            .observe(&body, Bytes::from(vec![b'x'; 7 * 1024 * 1024]))
            .is_err());
        assert!(signature.text.is_empty());
        assert!(signature.held.is_empty());
        assert!(signature.index.is_none());
    }
}

#[cfg(test)]
mod portable_input_tests {
    use super::*;
    use sandhi_core::{InMemorySink, KeyStore, RequestMetadataV1};
    use sandhi_providers::ProviderRuntime;
    use serde_json::json;
    #[tokio::test]
    async fn materialized_item_metadata_can_project_but_resource_lookup_cannot_cross_family() {
        let state = ProxyState::new(
            KeyStore::new(),
            crate::ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            Default::default(),
            None,
        );
        let vk = VirtualKey {
            id: "owner".into(),
            upstream_ref: "direct".into(),
            ..Default::default()
        };
        let provider = ProviderRuntime::new().openai_compat(
            "vendor",
            "http://127.0.0.1:1",
            "synthetic",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        );
        let request=crate::codec::decode_request(IngressDialect::Responses,json!({"model":"fixture","input":[{"id":"materialized_metadata","type":"message","role":"user","content":[{"type":"input_text","text":"full original input"}]}]}),RequestMetadataV1::default()).unwrap().0;
        assert!(
            prepare(&state, &vk, &provider, &request, IngressDialect::Responses)
                .await
                .unwrap()
                .is_none()
        );
        let sent = sandhi_providers::typed::encode_openai_request(&request).unwrap();
        assert!(!sent.to_string().contains("materialized_metadata"));
        assert!(sent.to_string().contains("full original input"));
        let request = crate::codec::decode_request(
            IngressDialect::Responses,
            json!({"model":"fixture","input":[{"type":"item_reference","id":"private_item"}]}),
            RequestMetadataV1::default(),
        )
        .unwrap()
        .0;
        assert!(
            prepare(&state, &vk, &provider, &request, IngressDialect::Responses)
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod resource_selector_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn private_builtin_resource_selectors_require_proof_while_creation_and_business_data_do_not() {
        for (family, body, count) in [
            (
                Family::OpenaiResponses,
                json!({"tools":[{"type":"file_search","vector_store_ids":["vs_private"]},{"type":"code_interpreter","container":{"type":"auto","file_ids":["file_private"]}},{"type":"mcp","connector_id":"connector_private"},{"type":"image_generation","input_image_mask":{"file_id":"file_mask"}}]}),
                4,
            ),
            (
                Family::AnthropicMessages,
                json!({"container":{"skills":[{"type":"custom","skill_id":"skill_private","version":"latest"}]}}),
                1,
            ),
            (
                Family::GeminiGenerateContent,
                json!({"tools":[{"fileSearch":{"fileSearchStoreNames":["fileSearchStores/private"]}},{"retrieval":{"vertexAiSearch":{"datastore":"projects/private/dataStores/store"}}},{"retrieval":{"vertexRagStore":{"ragResources":[{"ragCorpus":"ragCorpora/private","ragFileIds":["ragFiles/private"]}]}}}]}),
                4,
            ),
            (
                Family::GeminiGenerateContent,
                json!({"contents":[{"parts":[{"functionResponse":{"name":"tool","response":{"file_id":"ordinary-business-data"},"parts":[{"fileData":{"fileUri":"files/private"}}]}}]}]}),
                1,
            ),
        ] {
            assert_eq!(request_references(&body, family).unwrap().len(), count);
        }
        assert!(request_references(
            &json!({"tools":[{"type":"code_interpreter","container":"auto"}]}),
            Family::OpenaiResponses
        )
        .unwrap()
        .is_empty());
        assert!(request_references(&json!({"container":{"skills":[{"type":"anthropic","skill_id":"xlsx","version":"latest"}]}}),Family::AnthropicMessages).unwrap().is_empty());
        assert!(request_references(
            &json!({"tools":[{"type":"file_search","vector_store_ids":"single-private-id"}]}),
            Family::OpenaiResponses
        )
        .is_err());
        assert!(request_references(&json!({"tools":[{"retrieval":{"externalApi":{"endpoint":"https://external.invalid"}}}]}),Family::GeminiGenerateContent).is_err());
    }
}

#[cfg(test)]
mod text_endpoint_tests {
    use super::*;
    use sandhi_core::{InMemorySink, KeyStore, RequestMetadataV1};
    use sandhi_providers::ProviderRuntime;
    use serde_json::json;
    #[tokio::test]
    async fn text_and_embedding_planes_keep_their_original_materialized_response_contract() {
        let state = ProxyState::new(
            KeyStore::new(),
            crate::ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            Default::default(),
            None,
        );
        let vk = VirtualKey {
            id: "owner".into(),
            upstream_ref: "direct".into(),
            ..Default::default()
        };
        let provider = ProviderRuntime::new().openai_compat(
            "vendor",
            "http://127.0.0.1:1",
            "synthetic",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        );
        let request = crate::codec::decode_request(
            IngressDialect::OpenAi,
            json!({"model":"fixture","messages":[{"role":"user","content":"materialized text"}]}),
            RequestMetadataV1::default(),
        )
        .unwrap()
        .0;
        for dialect in [IngressDialect::Embeddings, IngressDialect::Completions] {
            let context = prepare(&state, &vk, &provider, &request, dialect)
                .await
                .unwrap();
            assert!(context.is_none());
            let mut response = Response::new(Body::from(vec![b'x'; LIMIT + 1]));
            response.extensions_mut().insert(NativeEmission(true));
            let bytes = to_bytes(protect(response, context).await.into_body(), LIMIT + 1)
                .await
                .unwrap();
            assert_eq!(bytes.len(), LIMIT + 1);
        }
    }
}

#[cfg(test)]
mod renewal_tests;
