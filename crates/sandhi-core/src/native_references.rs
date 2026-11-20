//! Recognize ownership-sensitive references in protocol-native JSON.
//! The gateway supplies issuer proofs. Direct SDK calls refuse references.

use crate::EndpointFamilyV1 as Family;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const LIMIT: usize = 8 * 1024 * 1024;
pub const RESPONSE: u8 = 1;
pub const ITEM: u8 = 2;
pub const ENCRYPTED: u8 = 3;
pub const FILE: u8 = 4;
pub const CACHE: u8 = 5;
pub const SIGNATURE: u8 = 6;
pub const REDACTED: u8 = 7;
pub const CONTAINER: u8 = 8;
pub const CONVERSATION: u8 = 9;
const PROMPT: u8 = 10;
const VECTOR: u8 = 11;
const AUDIO: u8 = 12;
const SKILL: u8 = 13;
const CONNECTOR: u8 = 14;
pub const UNVERIFIED_REFERENCE_MESSAGE:&str="native reference is unverified for this caller and backend; materialize the input or use state issued to this exact scope";
pub const MAX_BATCH: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    pub kind: u8,
    pub digest: String,
}

fn failure() -> &'static str {
    UNVERIFIED_REFERENCE_MESSAGE
}

#[derive(Default)]
pub struct References {
    values: Vec<Reference>,
    seen: HashSet<Reference>,
}
impl References {
    pub fn values(&self) -> &[Reference] {
        &self.values
    }

    pub fn into_values(self) -> Vec<Reference> {
        self.values
    }

    pub fn add(&mut self, kind: u8, value: &Value) -> Result<(), &'static str> {
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
pub fn request_references(body: &Value, family: Family) -> Result<Vec<Reference>, &'static str> {
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

pub fn response_refs(body: &Value, family: Family) -> Result<Vec<Reference>, &'static str> {
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
