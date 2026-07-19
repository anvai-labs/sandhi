//! Usage extraction — the metering-critical parsing. Each provider reports token usage
//! differently; getting the **cache split** right is what makes the meter trustworthy
//! (AnvaiOps ADR-0047 D10 / ADR-0020 D4). These are pure functions over the provider's real
//! response JSON — never estimates.

use crate::event::UsageEvent;
use serde_json::Value;

/// The token breakdown parsed from a provider response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParsedUsage {
    /// Fresh (non-cached) input tokens.
    pub tokens_in: u64,
    /// Completion tokens (finalized).
    pub tokens_out: u64,
    /// Prompt-cache write tokens (priced ~1.25x fresh input, e.g. Anthropic).
    pub cache_creation_tokens: u64,
    /// Prompt-cache read tokens (priced ~0.1x fresh input).
    pub cache_read_tokens: u64,
}

impl ParsedUsage {
    /// Stamp these counts onto an event (leaves attribution/metadata untouched).
    #[must_use]
    pub fn apply(self, event: UsageEvent) -> UsageEvent {
        event
            .with_tokens(self.tokens_in, self.tokens_out)
            .with_cache(self.cache_creation_tokens, self.cache_read_tokens)
    }
}

pub fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Parse an OpenAI (or OpenAI-compatible) Chat Completions response `usage` object.
///
/// `prompt_tokens` is the *total* prompt including cache; `prompt_tokens_details.cached_tokens`
/// is the cached portion — so fresh input = `prompt_tokens - cached_tokens`. OpenAI does not
/// bill cache writes separately, so `cache_creation_tokens` is 0. Returns `None` if there is no
/// `usage` object (e.g. an error body).
pub fn parse_openai_usage(response: &Value) -> Option<ParsedUsage> {
    let usage = response.get("usage")?;
    let prompt = u64_at(usage, "prompt_tokens");
    let completion = u64_at(usage, "completion_tokens");
    let cached = usage
        .get("prompt_tokens_details")
        .map(|d| u64_at(d, "cached_tokens"))
        .unwrap_or(0);
    Some(ParsedUsage {
        tokens_in: prompt.saturating_sub(cached),
        tokens_out: completion,
        cache_creation_tokens: 0,
        cache_read_tokens: cached,
    })
}

/// Parse an Anthropic Messages response `usage` object. Anthropic reports the cache split
/// directly: `input_tokens` is already the fresh (non-cached) input; cache writes and reads are
/// separate fields. Returns `None` if there is no `usage` object.
pub fn parse_anthropic_usage(response: &Value) -> Option<ParsedUsage> {
    let usage = response.get("usage")?;
    Some(ParsedUsage {
        tokens_in: u64_at(usage, "input_tokens"),
        tokens_out: u64_at(usage, "output_tokens"),
        cache_creation_tokens: u64_at(usage, "cache_creation_input_tokens"),
        cache_read_tokens: u64_at(usage, "cache_read_input_tokens"),
    })
}

/// Parse a Google Gemini `generateContent` response `usageMetadata`. `promptTokenCount` is the
/// full prompt including any cached content; `cachedContentTokenCount` is the cached portion, so
/// fresh input = prompt − cached. Returns `None` if there is no `usageMetadata`.
pub fn parse_gemini_usage(response: &Value) -> Option<ParsedUsage> {
    let usage = response.get("usageMetadata")?;
    let prompt = u64_at(usage, "promptTokenCount");
    let cached = u64_at(usage, "cachedContentTokenCount");
    Some(ParsedUsage {
        tokens_in: prompt.saturating_sub(cached),
        tokens_out: u64_at(usage, "candidatesTokenCount"),
        cache_creation_tokens: 0,
        cache_read_tokens: cached,
    })
}

/// Parse a Cohere v2 chat response `usage` object. Prefers `billed_units` (what you are billed);
/// falls back to `tokens`. Cohere has no prompt-cache split. Returns `None` if there is no
/// `usage` object.
pub fn parse_cohere_usage(response: &Value) -> Option<ParsedUsage> {
    let usage = response.get("usage")?;
    let units = usage
        .get("billed_units")
        .or_else(|| usage.get("tokens"))
        .unwrap_or(usage);
    Some(ParsedUsage {
        tokens_in: u64_at(units, "input_tokens"),
        tokens_out: u64_at(units, "output_tokens"),
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
    })
}

/// Parse an Ollama native `/api/chat` (or `/api/generate`) response. `prompt_eval_count` is the
/// input tokens, `eval_count` the output. No cache split. Returns `None` if neither field is
/// present. (vLLM and other OpenAI-compatible local servers use [`parse_openai_usage`] instead.)
pub fn parse_ollama_usage(response: &Value) -> Option<ParsedUsage> {
    if response.get("prompt_eval_count").is_none() && response.get("eval_count").is_none() {
        return None;
    }
    Some(ParsedUsage {
        tokens_in: u64_at(response, "prompt_eval_count"),
        tokens_out: u64_at(response, "eval_count"),
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
    })
}

/// Parse an AWS Bedrock response body. Model-dependent: Anthropic-on-Bedrock carries a
/// `usage.{input_tokens,output_tokens}` object; Amazon Titan carries `inputTextTokenCount` +
/// `results[].tokenCount`. (The reliable cross-model source is the response's
/// `X-Amzn-Bedrock-*-Token-Count` **headers**, handled by the transport, not this body parser.)
/// Returns `None` if no recognized shape is present.
pub fn parse_bedrock_usage(response: &Value) -> Option<ParsedUsage> {
    if let Some(usage) = response.get("usage") {
        return Some(ParsedUsage {
            tokens_in: u64_at(usage, "input_tokens"),
            tokens_out: u64_at(usage, "output_tokens"),
            cache_creation_tokens: u64_at(usage, "cache_creation_input_tokens"),
            cache_read_tokens: u64_at(usage, "cache_read_input_tokens"),
        });
    }
    if response.get("inputTextTokenCount").is_some() {
        let out = response
            .get("results")
            .and_then(Value::as_array)
            .map(|r| r.iter().map(|x| u64_at(x, "tokenCount")).sum())
            .unwrap_or(0);
        return Some(ParsedUsage {
            tokens_in: u64_at(response, "inputTextTokenCount"),
            tokens_out: out,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Backend;
    use serde_json::json;

    #[test]
    fn openai_splits_cached_from_fresh_input() {
        let resp = json!({
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 200,
                "prompt_tokens_details": { "cached_tokens": 800 }
            }
        });
        let u = parse_openai_usage(&resp).unwrap();
        assert_eq!(u.tokens_in, 200); // 1000 total - 800 cached
        assert_eq!(u.tokens_out, 200);
        assert_eq!(u.cache_read_tokens, 800);
        assert_eq!(u.cache_creation_tokens, 0);
    }

    #[test]
    fn openai_without_cache_details() {
        let resp = json!({ "usage": { "prompt_tokens": 50, "completion_tokens": 10 } });
        let u = parse_openai_usage(&resp).unwrap();
        assert_eq!(u.tokens_in, 50);
        assert_eq!(u.cache_read_tokens, 0);
    }

    #[test]
    fn openai_error_body_has_no_usage() {
        let resp = json!({ "error": { "message": "bad key" } });
        assert!(parse_openai_usage(&resp).is_none());
    }

    #[test]
    fn anthropic_reports_cache_split_directly() {
        let resp = json!({
            "usage": {
                "input_tokens": 120,
                "output_tokens": 45,
                "cache_creation_input_tokens": 300,
                "cache_read_input_tokens": 900
            }
        });
        let u = parse_anthropic_usage(&resp).unwrap();
        assert_eq!(u.tokens_in, 120);
        assert_eq!(u.tokens_out, 45);
        assert_eq!(u.cache_creation_tokens, 300);
        assert_eq!(u.cache_read_tokens, 900);
    }

    #[test]
    fn parsed_usage_stamps_onto_event_without_touching_attribution() {
        let base = UsageEvent::new("r", "t", "anthropic", "claude-x", Backend::External)
            .with_attribution(Some("vk".into()), Some("alice".into()), None);
        let resp = json!({ "usage": { "input_tokens": 10, "output_tokens": 5,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 2 } });
        let ev = parse_anthropic_usage(&resp).unwrap().apply(base);
        assert_eq!(ev.subject_id.as_deref(), Some("alice"));
        assert_eq!(ev.tokens_in, 10);
        assert_eq!(ev.cache_read_tokens, 2);
        assert_eq!(ev.billable_tokens(), 15);
    }

    #[test]
    fn gemini_splits_cached_content() {
        let resp = json!({ "usageMetadata": {
            "promptTokenCount": 100, "candidatesTokenCount": 30, "cachedContentTokenCount": 40
        }});
        let u = parse_gemini_usage(&resp).unwrap();
        assert_eq!(u.tokens_in, 60); // 100 - 40 cached
        assert_eq!(u.tokens_out, 30);
        assert_eq!(u.cache_read_tokens, 40);
    }

    #[test]
    fn cohere_prefers_billed_units() {
        let resp = json!({ "usage": {
            "billed_units": { "input_tokens": 12, "output_tokens": 8 },
            "tokens": { "input_tokens": 15, "output_tokens": 8 }
        }});
        let u = parse_cohere_usage(&resp).unwrap();
        assert_eq!(u.tokens_in, 12); // billed_units, not tokens
        assert_eq!(u.tokens_out, 8);
    }

    #[test]
    fn ollama_reads_eval_counts() {
        let resp = json!({ "prompt_eval_count": 26, "eval_count": 14, "done": true });
        let u = parse_ollama_usage(&resp).unwrap();
        assert_eq!(u.tokens_in, 26);
        assert_eq!(u.tokens_out, 14);
        assert!(parse_ollama_usage(&json!({ "done": true })).is_none());
    }

    #[test]
    fn bedrock_anthropic_and_titan_shapes() {
        let anthropic = json!({ "usage": { "input_tokens": 11, "output_tokens": 4 } });
        let u = parse_bedrock_usage(&anthropic).unwrap();
        assert_eq!((u.tokens_in, u.tokens_out), (11, 4));

        let titan = json!({ "inputTextTokenCount": 20, "results": [{ "tokenCount": 7 }] });
        let t = parse_bedrock_usage(&titan).unwrap();
        assert_eq!((t.tokens_in, t.tokens_out), (20, 7));

        assert!(parse_bedrock_usage(&json!({ "other": 1 })).is_none());
    }
}
