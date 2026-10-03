//! Accounting-grade plaintext decode for raw-only OpenAI-compatible non-chat routes.
//! The canonical request never reaches a chat encoder; its output maximum is the
//! total reservation exposure across prompt batches and generated alternatives.
use crate::codec::{decode_openai_request, IngressDialect};
use sandhi_core::{ChatRequestV1, RequestMetadataV1};
use serde_json::{json, Map, Value};

fn integer(
    object: &Map<String, Value>,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let value = match object.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| format!("{key} must be an unsigned integer"))?,
    };
    if !(min..=max).contains(&value) {
        return Err(format!("{key} is outside the supported range"));
    }
    Ok(value)
}
fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<bool, String> {
    match object.get(key) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| format!("{key} must be boolean")),
    }
}

pub(crate) fn decode(
    dialect: IngressDialect,
    body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    let object = body.as_object().ok_or("request must be an object")?;
    let embeddings = dialect == IngressDialect::Embeddings;
    let allowed: &[&str] = if embeddings {
        &["model", "input", "encoding_format", "dimensions", "user"]
    } else {
        &[
            "model",
            "prompt",
            "suffix",
            "max_tokens",
            "temperature",
            "top_p",
            "n",
            "stream",
            "stream_options",
            "logprobs",
            "echo",
            "stop",
            "presence_penalty",
            "frequency_penalty",
            "best_of",
            "logit_bias",
            "user",
            "seed",
        ]
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("unsupported field on plaintext endpoint".into());
    }
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .ok_or("model must be a nonempty string")?;
    let field = if embeddings { "input" } else { "prompt" };
    let texts: Vec<&str> = match object.get(field) {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(texts)) if !texts.is_empty() && texts.len() <= 2048 => texts
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or("token IDs and mixed batches are not supported")
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(format!(
                "{field} must be text or a nonempty text batch (at most 2048 items)"
            ))
        }
    };
    if embeddings && texts.iter().any(|text| text.is_empty()) {
        return Err("embedding text must not be empty".into());
    }
    for key in ["user", "suffix"] {
        if object.get(key).is_some_and(|v| !v.is_string()) {
            return Err(format!("{key} must be text"));
        }
    }
    let mut alternatives = 1;
    let mut output = 0;
    let mut stream = false;
    if embeddings {
        if let Some(value) = object.get("encoding_format") {
            if !matches!(value.as_str(), Some("float" | "base64")) {
                return Err("unsupported embedding encoding_format".into());
            }
        }
        if object.contains_key("dimensions") {
            integer(object, "dimensions", 1, 1, 65536)?;
        }
    } else {
        stream = optional_bool(object, "stream")?;
        optional_bool(object, "echo")?;
        let n = integer(object, "n", 1, 1, 128)?;
        alternatives = integer(object, "best_of", n, 1, 128)?;
        if alternatives < n {
            return Err("best_of must be at least n".into());
        }
        let max_tokens = integer(object, "max_tokens", 16, 0, 1_048_576)?;
        output = max_tokens
            .checked_mul(texts.len() as u64)
            .and_then(|v| v.checked_mul(alternatives))
            .ok_or("completion reservation overflow")?;
        for (key, min, max) in [
            ("temperature", 0.0, 2.0),
            ("top_p", 0.0, 1.0),
            ("presence_penalty", -2.0, 2.0),
            ("frequency_penalty", -2.0, 2.0),
        ] {
            if let Some(value) = object.get(key) {
                let v = value
                    .as_f64()
                    .ok_or_else(|| format!("{key} must be numeric"))?;
                if !(min..=max).contains(&v) {
                    return Err(format!("{key} is outside the supported range"));
                }
            }
        }
        if object.contains_key("logprobs") {
            integer(object, "logprobs", 0, 0, 100)?;
        }
        if object.get("seed").is_some_and(|v| v.as_i64().is_none()) {
            return Err("seed must be a signed integer".into());
        }
        if let Some(stop) = object.get("stop") {
            if !stop.is_string()
                && !stop
                    .as_array()
                    .is_some_and(|a| a.len() <= 4 && a.iter().all(Value::is_string))
            {
                return Err("stop must be text or at most four text strings".into());
            }
        }
        if let Some(options) = object.get("stream_options") {
            let options = options
                .as_object()
                .ok_or("stream_options must be an object")?;
            if options.keys().any(|k| k != "include_usage") {
                return Err("unsupported stream option".into());
            }
            optional_bool(options, "include_usage")?;
            if !stream {
                return Err("stream_options requires stream=true".into());
            }
        }
        if let Some(bias) = object.get("logit_bias") {
            let bias = bias.as_object().ok_or("logit_bias must be an object")?;
            if bias.len() > 4096
                || bias.iter().any(|(key, value)| {
                    key.parse::<u32>().is_err()
                        || !value
                            .as_f64()
                            .is_some_and(|v| (-100.0..=100.0).contains(&v))
                })
            {
                return Err("invalid logit_bias".into());
            }
        }
    }
    let messages: Vec<_> = texts
        .iter()
        .map(|text| json!({"role":"user","content":text}))
        .collect();
    // Only admission/accounting sees this view. Dispatch preserves original endpoint bytes,
    // except the explicitly documented default max_tokens insertion for completions.
    let (mut request, _) = decode_openai_request(
        json!({"model":model,"messages":messages,"max_tokens":output}),
        metadata,
    )?;
    request.extensions.clear();
    request
        .extensions
        .insert("sandhi.input_multiplier".into(), json!(alternatives));
    let suffix_bytes = object
        .get("suffix")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    request.extensions.insert(
        "sandhi.repeated_suffix_bytes".into(),
        json!(suffix_bytes.saturating_mul(texts.len().saturating_sub(1))),
    );
    Ok((request, stream))
}

/// Count deterministic repeated context before applying the existing token estimate.
/// No repeated strings are allocated, and the same exposure is used for calibration.
pub(crate) fn input_bytes(request: &ChatRequestV1, wire_bytes: usize) -> usize {
    let read = |key: &str, default| {
        request
            .extensions
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or(default)
    };
    let expanded = (wire_bytes as u64)
        .saturating_add(read("sandhi.repeated_suffix_bytes", 0))
        .saturating_mul(read("sandhi.input_multiplier", 1));
    usize::try_from(expanded).unwrap_or(usize::MAX)
}
