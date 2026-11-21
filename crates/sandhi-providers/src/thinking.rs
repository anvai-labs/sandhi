//! Normalize vendor thinking controls so ingress and direct admission agree.

use sandhi_core::chat::ThinkingRequestV1;
use serde_json::{Map, Value};

pub fn anthropic_effort_only(body: &Map<String, Value>) -> bool {
    body.get("thinking").is_none_or(Value::is_null)
        && body
            .get("output_config")
            .and_then(|config| config.get("effort"))
            .is_some_and(|effort| !effort.is_null())
}

pub fn decode_anthropic_thinking(
    body: &Map<String, Value>,
) -> Result<Option<ThinkingRequestV1>, &'static str> {
    let output = body.get("output_config").filter(|value| !value.is_null());
    let effort = output
        .map(|output| output.as_object().ok_or("output_config must be an object"))
        .transpose()?
        .and_then(|output| output.get("effort"))
        .filter(|effort| !effort.is_null())
        .map(|effort| {
            effort
                .as_str()
                .filter(|effort| !effort.is_empty())
                .ok_or("output_config.effort must be a nonempty string")
        })
        .transpose()?;
    let Some(config) = body.get("thinking").filter(|value| !value.is_null()) else {
        return Ok(effort.map(|value| ThinkingRequestV1::enabled().with_effort(value)));
    };
    let config = config.as_object().ok_or("thinking must be an object")?;
    let mode = config.get("type").and_then(Value::as_str);
    if mode == Some("adaptive") {
        if config
            .keys()
            .any(|key| !matches!(key.as_str(), "type" | "display"))
        {
            return Err("adaptive thinking supports only type and display");
        }
        if config
            .get("display")
            .filter(|value| !value.is_null())
            .is_some_and(|value| !matches!(value.as_str(), Some("summarized" | "omitted")))
        {
            return Err("adaptive thinking display must be summarized or omitted");
        }
        let request = ThinkingRequestV1::enabled();
        return Ok(Some(match effort {
            Some(value) => request.with_effort(value),
            None => request,
        }));
    }
    if effort.is_some() {
        return Err("output_config.effort requires adaptive thinking");
    }
    if config
        .keys()
        .any(|key| !matches!(key.as_str(), "type" | "budget_tokens"))
    {
        return Err("thinking contains unsupported fields");
    }
    let budget = config.get("budget_tokens");
    match mode {
        Some("enabled") => budget
            .and_then(Value::as_u64)
            .filter(|budget| *budget > 0)
            .map(|budget| Some(ThinkingRequestV1::enabled().with_budget(budget)))
            .ok_or("enabled thinking requires a positive integer budget_tokens"),
        Some("disabled") if budget.is_none() => Ok(Some(ThinkingRequestV1::disabled())),
        Some("disabled") => Err("disabled thinking cannot carry a budget"),
        _ => Err("thinking.type must be adaptive, enabled or disabled"),
    }
}

fn config_alias<'a>(
    object: &'a Map<String, Value>,
    json_name: &str,
    proto_name: &str,
    duplicate_error: &'static str,
    type_error: &'static str,
) -> Result<Option<&'a Map<String, Value>>, &'static str> {
    if object.contains_key(json_name) && object.contains_key(proto_name) {
        return Err(duplicate_error);
    }
    object
        .get(json_name)
        .or_else(|| object.get(proto_name))
        .filter(|value| !value.is_null())
        .map(|value| value.as_object().ok_or(type_error))
        .transpose()
}

pub fn gemini_generation_config(
    body: &Map<String, Value>,
) -> Result<Option<&Map<String, Value>>, &'static str> {
    config_alias(
        body,
        "generationConfig",
        "generation_config",
        "Gemini generation config spellings must not both be supplied",
        "Gemini generationConfig must be an object",
    )
}

pub fn gemini_thinking_config(
    generation: &Map<String, Value>,
) -> Result<Option<&Map<String, Value>>, &'static str> {
    config_alias(
        generation,
        "thinkingConfig",
        "thinking_config",
        "Gemini thinking config spellings must not both be supplied",
        "Gemini thinkingConfig must be an object",
    )
}

pub fn is_gemini_thinking_field(name: &str) -> bool {
    matches!(
        name,
        "thinkingBudget" | "thinking_budget" | "thinkingLevel" | "thinking_level"
    )
}

pub fn decode_gemini_thinking(
    body: &Map<String, Value>,
) -> Result<Option<ThinkingRequestV1>, &'static str> {
    let Some(config) = gemini_generation_config(body)?
        .map(gemini_thinking_config)
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    if config.contains_key("thinkingBudget") && config.contains_key("thinking_budget") {
        return Err("Gemini thinking budget spellings must not both be supplied");
    }
    if config.contains_key("thinkingLevel") && config.contains_key("thinking_level") {
        return Err("Gemini thinking level spellings must not both be supplied");
    }
    let budget = config
        .get("thinkingBudget")
        .or_else(|| config.get("thinking_budget"))
        .filter(|value| !value.is_null());
    let level = config
        .get("thinkingLevel")
        .or_else(|| config.get("thinking_level"))
        .filter(|value| !value.is_null())
        .map(|value| match value.as_str() {
            Some("THINKING_LEVEL_UNSPECIFIED") => Ok(None),
            Some("MINIMAL") => Ok(Some("minimal")),
            Some("LOW") => Ok(Some("low")),
            Some("MEDIUM") => Ok(Some("medium")),
            Some("HIGH") => Ok(Some("high")),
            Some("XHIGH") => Ok(Some("xhigh")),
            _ => Err("unknown Gemini thinkingLevel"),
        })
        .transpose()?
        .flatten();
    match (level, budget) {
        (Some(_), Some(_)) => Err("Gemini thinkingLevel and thinkingBudget are mutually exclusive"),
        (Some(level), None) => Ok(Some(ThinkingRequestV1::enabled().with_effort(level))),
        (None, Some(budget)) => match budget.as_u64() {
            Some(0) => Ok(Some(ThinkingRequestV1::disabled())),
            Some(budget) => Ok(Some(ThinkingRequestV1::enabled().with_budget(budget))),
            None => Err("Gemini thinkingBudget must be a non-negative integer"),
        },
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_preserves_adaptive_enable_with_default_effort() {
        let body = json!({"thinking":{"type":"adaptive"},"output_config":{"effort":null}});
        assert_eq!(
            decode_anthropic_thinking(body.as_object().unwrap()),
            Ok(Some(ThinkingRequestV1::enabled()))
        );
        let body = json!({"thinking":null,"output_config":{"effort":"high"}});
        assert_eq!(
            decode_anthropic_thinking(body.as_object().unwrap()),
            Ok(Some(ThinkingRequestV1::enabled().with_effort("high")))
        );
    }

    #[test]
    fn gemini_defaults_and_conflicting_controls_are_distinct() {
        let config = |thinking| json!({"generationConfig":{"thinkingConfig":thinking}});
        for value in [Value::Null, json!("THINKING_LEVEL_UNSPECIFIED")] {
            let body = config(json!({"thinkingLevel":value}));
            assert_eq!(decode_gemini_thinking(body.as_object().unwrap()), Ok(None));
        }
        let body = config(json!({"thinkingLevel":"LOW","thinkingBudget":0}));
        assert!(decode_gemini_thinking(body.as_object().unwrap()).is_err());
        let body = config(json!({"thinking_level":"LOW"}));
        assert_eq!(
            decode_gemini_thinking(body.as_object().unwrap()),
            Ok(Some(ThinkingRequestV1::enabled().with_effort("low")))
        );
        let body = config(json!({"thinkingLevel":"LOW","thinking_level":"LOW"}));
        assert!(decode_gemini_thinking(body.as_object().unwrap()).is_err());
    }

    #[test]
    fn gemini_reads_proto_container_names_and_rejects_duplicates() {
        let mut body = json!({"generation_config":{"thinking_config":{"thinking_level":"LOW"}}});
        assert_eq!(
            decode_gemini_thinking(body.as_object().unwrap()),
            Ok(Some(ThinkingRequestV1::enabled().with_effort("low")))
        );
        body["generation_config"]["thinkingConfig"] = json!(null);
        assert!(decode_gemini_thinking(body.as_object().unwrap()).is_err());
        body["generationConfig"] = json!(null);
        assert!(decode_gemini_thinking(body.as_object().unwrap()).is_err());
    }
}
