//! Usage extraction — the metering-critical parsing. Each provider reports token usage
//! differently; getting the **cache split** right is what makes the meter trustworthy
//! (AnvaiOps ADR-0047 D10 / ADR-0020 D4). These are pure functions over the provider's real
//! response JSON — never estimates.

use crate::event::UsageEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The token breakdown parsed from a provider response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ParsedUsage {
    /// Fresh (non-cached) input tokens.
    pub tokens_in: u64,
    /// Completion tokens (finalized).
    pub tokens_out: u64,
    /// Prompt-cache write tokens (priced ~1.25x fresh input, e.g. Anthropic).
    pub cache_creation_tokens: u64,
    /// Prompt-cache read tokens (priced ~0.1x fresh input).
    pub cache_read_tokens: u64,
    /// Reasoning tokens when reported separately (0 when folded into `tokens_out`
    /// or not reported).
    pub reasoning_tokens: u64,
}

impl From<ParsedUsage> for crate::chat::UsageV2 {
    fn from(value: ParsedUsage) -> Self {
        Self {
            tokens_in: value.tokens_in,
            tokens_out: value.tokens_out,
            cache_creation_tokens: value.cache_creation_tokens,
            cache_read_tokens: value.cache_read_tokens,
            reasoning_tokens: (value.reasoning_tokens > 0).then_some(value.reasoning_tokens),
            completeness: crate::chat::UsageCompleteness::Final,
            ..Self::default()
        }
    }
}

impl ParsedUsage {
    /// Stamp these counts onto an event (leaves attribution/metadata untouched).
    ///
    /// Measurement (`outcome` / `usage_completeness` / `attempts`) is **filled if empty**, never
    /// overwritten — a caller that already set the real outcome (e.g. the proxy metering layer,
    /// which stamps `"error"`/`"cancelled"` after this) keeps it. A caller that set nothing gets
    /// the success/Final defaults. This makes `apply` safe to compose with explicit measurement:
    /// previously it unconditionally stamped `success`/`Final`, clobbering any caller-set value.
    #[must_use]
    pub fn apply(self, mut event: UsageEvent) -> UsageEvent {
        event = event
            .with_tokens(self.tokens_in, self.tokens_out)
            .with_cache(self.cache_creation_tokens, self.cache_read_tokens)
            .with_reasoning((self.reasoning_tokens > 0).then_some(self.reasoning_tokens));
        if event.usage_completeness == crate::chat::UsageCompleteness::Unavailable {
            event.usage_completeness = crate::chat::UsageCompleteness::Final;
        }
        event.attempts = event.attempts.max(1);
        event.outcome.get_or_insert_with(|| "success".to_string());
        event
    }
}

/// A per-call token count beyond this is not physically plausible (the largest context windows are
/// in the millions of tokens). Treating a larger value as `0` + a warning keeps a malformed or
/// adversarial upstream from injecting garbage into the meter; the ceiling sits ~1000× above any
/// real call, so legitimate counts are unaffected.
const MAX_PLAUSIBLE_TOKENS: u64 = 50_000_000_000;

/// Read an unsigned integer at `key`, defaulting to `0`. Absurd values (above
/// [`MAX_PLAUSIBLE_TOKENS`]) clamp to `0` with a warning rather than being trusted — they would
/// otherwise distort `billable` and the budget.
pub fn u64_at(v: &Value, key: &str) -> u64 {
    match v.get(key).and_then(Value::as_u64) {
        Some(n) if n <= MAX_PLAUSIBLE_TOKENS => n,
        Some(n) => {
            tracing::warn!(
                target: "sandhi.usage.absurd_count",
                key, count = n, max = MAX_PLAUSIBLE_TOKENS,
                "usage field exceeds the plausible per-call ceiling; clamping to 0"
            );
            0
        }
        None => 0,
    }
}

/// Split a total prompt token count into `(fresh_input, inconsistent)` given the cached portion.
/// When `cached > prompt` (a malformed/inconsistent usage shape) fresh input saturates to `0` and
/// `inconsistent` is `true` so the caller can surface it — a silent zero-fresh-input is exactly
/// the failure mode that hides a broken cache split from the meter.
fn split_prompt(prompt: u64, cached: u64) -> (u64, bool) {
    if cached > prompt {
        (0, true)
    } else {
        (prompt - cached, false)
    }
}

/// Parse an OpenAI (or OpenAI-compatible) Chat Completions response `usage` object.
///
/// `prompt_tokens` is the *total* prompt including cache; `prompt_tokens_details.cached_tokens`
/// is the cached portion — so fresh input = `prompt_tokens - cached_tokens`. OpenAI does not
/// bill cache writes separately, so `cache_creation_tokens` is 0. Returns `None` if there is no
/// `usage` object (e.g. an error body).
pub fn parse_openai_usage(response: &Value) -> Option<ParsedUsage> {
    let usage = response.get("usage")?;
    let completion = u64_at(usage, "completion_tokens");
    let reasoning = usage
        .get("completion_tokens_details")
        .map(|d| u64_at(d, "reasoning_tokens"))
        .unwrap_or(0);
    // DeepSeek (and other OpenAI-compat vendors) report cache as top-level hit/miss counts rather
    // than OpenAI's nested `prompt_tokens_details.cached_tokens`. When present, take that branch
    // exclusively: `prompt_tokens == hit + miss`, so fresh input is `miss` and cache_read is `hit`.
    // (catalog.rs declares `prompt_cache_usage: true` for these vendors — this honors it.)
    if usage.get("prompt_cache_hit_tokens").is_some() {
        let hit = u64_at(usage, "prompt_cache_hit_tokens");
        let fresh = if usage.get("prompt_cache_miss_tokens").is_some() {
            u64_at(usage, "prompt_cache_miss_tokens")
        } else {
            u64_at(usage, "prompt_tokens").saturating_sub(hit)
        };
        return Some(ParsedUsage {
            tokens_in: fresh,
            tokens_out: completion,
            cache_creation_tokens: 0,
            cache_read_tokens: hit,
            reasoning_tokens: reasoning,
        });
    }
    let prompt = u64_at(usage, "prompt_tokens");
    let cached = usage
        .get("prompt_tokens_details")
        .map(|d| u64_at(d, "cached_tokens"))
        .unwrap_or(0);
    let (tokens_in, inconsistent) = split_prompt(prompt, cached);
    if inconsistent {
        tracing::warn!(
            target: "sandhi.usage.inconsistent_cache",
            prompt, cached,
            "OpenAI usage reports cached_tokens > prompt_tokens; clamping fresh input to 0"
        );
    }
    Some(ParsedUsage {
        tokens_in,
        tokens_out: completion,
        cache_creation_tokens: 0,
        cache_read_tokens: cached,
        reasoning_tokens: reasoning,
    })
}

/// Parse an OpenAI Responses API `usage` object.
///
/// Responses uses `input_tokens` / `output_tokens` rather than the Chat Completions
/// `prompt_tokens` / `completion_tokens` names. `input_tokens` includes cached input, so the
/// neutral fresh-input count subtracts `input_tokens_details.cached_tokens` exactly once.
pub fn parse_openai_responses_usage(response: &Value) -> Option<ParsedUsage> {
    let usage = response.get("usage")?;
    let input = u64_at(usage, "input_tokens");
    let output = u64_at(usage, "output_tokens");
    let cached = usage
        .get("input_tokens_details")
        .map(|details| u64_at(details, "cached_tokens"))
        .unwrap_or(0);
    let (tokens_in, inconsistent) = split_prompt(input, cached);
    if inconsistent {
        tracing::warn!(
            target: "sandhi.usage.inconsistent_cache",
            prompt = input, cached,
            "Responses usage reports cached_tokens > input_tokens; clamping fresh input to 0"
        );
    }
    let reasoning = usage
        .get("output_tokens_details")
        .map(|details| u64_at(details, "reasoning_tokens"))
        .unwrap_or(0);
    Some(ParsedUsage {
        tokens_in,
        tokens_out: output,
        cache_creation_tokens: 0,
        cache_read_tokens: cached,
        reasoning_tokens: reasoning,
    })
}

/// Parse an Anthropic Messages response `usage` object. Anthropic reports the cache split
/// directly: `input_tokens` is already the fresh (non-cached) input; cache writes and reads are
/// separate fields. Returns `None` if there is no `usage` object.
///
/// TD-0001 W3 pilot (ADR-0003 §2/§4): the `usage` shape is deserialized through the
/// **typify-generated** narrow model [`crate::generated::anthropic_usage::AnthropicMessageUsage`]
/// (regenerated from the byte-pinned schema by `scripts/gen-provider-models.sh`, never
/// hand-edited); this function is the hand-written overlay that maps it onto [`ParsedUsage`]. All
/// fields are optional in the schema, so a missing field defaults to `0` — the same lenient
/// behavior as the prior `u64_at` extraction.
pub fn parse_anthropic_usage(response: &Value) -> Option<ParsedUsage> {
    let usage = response.get("usage")?;
    let u: crate::generated::anthropic_usage::AnthropicMessageUsage =
        serde_json::from_value(usage.clone()).ok()?;
    Some(ParsedUsage {
        tokens_in: u.input_tokens.unwrap_or(0).max(0) as u64,
        tokens_out: u.output_tokens.unwrap_or(0).max(0) as u64,
        cache_creation_tokens: u.cache_creation_input_tokens.unwrap_or(0).max(0) as u64,
        cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0).max(0) as u64,
        reasoning_tokens: 0, // Anthropic folds thinking tokens into output_tokens
    })
}

/// Parse a Google Gemini `generateContent` response `usageMetadata`. `promptTokenCount` is the
/// full prompt including any cached content; `cachedContentTokenCount` is the cached portion, so
/// fresh input = prompt − cached. Returns `None` if there is no `usageMetadata`.
pub fn parse_gemini_usage(response: &Value) -> Option<ParsedUsage> {
    let usage = response.get("usageMetadata")?;
    let prompt = u64_at(usage, "promptTokenCount");
    let cached = u64_at(usage, "cachedContentTokenCount");
    let (tokens_in, inconsistent) = split_prompt(prompt, cached);
    if inconsistent {
        tracing::warn!(
            target: "sandhi.usage.inconsistent_cache",
            prompt, cached,
            "Gemini usageMetadata reports cachedContentTokenCount > promptTokenCount; clamping fresh input to 0"
        );
    }
    Some(ParsedUsage {
        tokens_in,
        tokens_out: u64_at(usage, "candidatesTokenCount"),
        cache_creation_tokens: 0,
        cache_read_tokens: cached,
        reasoning_tokens: u64_at(usage, "thoughtsTokenCount"),
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
    let tokens_in = u64_at(units, "input_tokens");
    let tokens_out = u64_at(units, "output_tokens");
    // No recognizable token field anywhere ⇒ not a usage shape we can meter faithfully. Return
    // `None` rather than fabricating a zero-token event (a silent, wrong measurement).
    if tokens_in == 0 && tokens_out == 0 {
        return None;
    }
    Some(ParsedUsage {
        tokens_in,
        tokens_out,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        reasoning_tokens: 0,
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
        reasoning_tokens: 0,
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
            reasoning_tokens: 0,
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
            reasoning_tokens: 0,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Backend;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[test]
    fn openai_parses_separately_reported_reasoning_tokens() {
        let resp = json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "completion_tokens_details": { "reasoning_tokens": 30 }
            }
        });
        assert_eq!(parse_openai_usage(&resp).unwrap().reasoning_tokens, 30);
    }

    #[test]
    fn openai_responses_parses_reasoning_tokens() {
        let resp = json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "output_tokens_details": { "reasoning_tokens": 12 }
            }
        });
        assert_eq!(
            parse_openai_responses_usage(&resp)
                .unwrap()
                .reasoning_tokens,
            12
        );
    }

    #[test]
    fn gemini_parses_thoughts_token_count_as_reasoning() {
        let resp = json!({
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 40,
                "thoughtsTokenCount": 25
            }
        });
        assert_eq!(parse_gemini_usage(&resp).unwrap().reasoning_tokens, 25);
    }

    #[test]
    fn apply_carries_reasoning_onto_event_and_zero_stays_none() {
        let base = UsageEvent::new(
            "r1",
            "2026-01-01T00:00:00Z",
            "openai",
            "o4",
            Backend::External,
        );
        let with = ParsedUsage {
            tokens_in: 1,
            tokens_out: 2,
            reasoning_tokens: 9,
            ..Default::default()
        }
        .apply(base.clone());
        assert_eq!(with.reasoning_tokens, Some(9));

        let without = ParsedUsage {
            tokens_in: 1,
            tokens_out: 2,
            ..Default::default()
        }
        .apply(base);
        assert_eq!(without.reasoning_tokens, None);
    }

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
    fn openai_responses_uses_distinct_names_and_splits_cached_input() {
        let response = json!({
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 200,
                "input_tokens_details": {"cached_tokens": 800}
            }
        });
        let usage = parse_openai_responses_usage(&response).unwrap();
        assert_eq!(usage.tokens_in, 200);
        assert_eq!(usage.tokens_out, 200);
        assert_eq!(usage.cache_read_tokens, 800);
        assert_eq!(usage.cache_creation_tokens, 0);
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
        // 10 fresh in + 0 cache-creation + 2 cache-read + 5 out (D4); narrow read 15.
        assert_eq!(ev.billable_tokens(), 17);
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

    // ── Scope 1 hardening: overflow-safe counts, absurd-value clamping, non-canonical cache,
    //    observable inconsistency, non-destructive apply, Cohere no-zeros. ──

    #[test]
    fn u64_at_rejects_absurd_values() {
        // A plausible count passes through unchanged.
        assert_eq!(u64_at(&json!({ "n": 1234 }), "n"), 1234);
        // A u64 that fits but is physically absurd (no single call has 200B tokens) clamps to 0
        // rather than being trusted into the meter (would overflow/distort billable).
        assert_eq!(u64_at(&json!({ "n": 200_000_000_000u64 }), "n"), 0);
        // A float-shaped absurd value likewise resolves to 0.
        assert_eq!(u64_at(&json!({ "n": 1e20 }), "n"), 0);
        // Absent key → 0 (unchanged).
        assert_eq!(u64_at(&json!({ "other": 1 }), "n"), 0);
    }

    #[test]
    fn split_prompt_splits_and_flags_inconsistency() {
        assert_eq!(split_prompt(1000, 800), (200, false));
        assert_eq!(split_prompt(100, 0), (100, false));
        // cached > prompt ⇒ fresh input saturates to 0 AND the inconsistency flag is set.
        assert_eq!(split_prompt(100, 500), (0, true));
        assert_eq!(split_prompt(0, 1), (0, true));
    }

    #[test]
    fn parse_openai_usage_reads_deepseek_prompt_cache() {
        // DeepSeek reports cache as top-level hit/miss, not OpenAI's nested cached_tokens.
        let resp = json!({
            "usage": {
                "prompt_tokens": 1000,
                "prompt_cache_hit_tokens": 800,
                "prompt_cache_miss_tokens": 200,
                "completion_tokens": 50
            }
        });
        let u = parse_openai_usage(&resp).unwrap();
        assert_eq!(u.cache_read_tokens, 800); // honored, not zeroed
        assert_eq!(u.tokens_in, 200); // fresh = miss, not prompt(1000) billed as fresh
        assert_eq!(u.tokens_out, 50);
        assert_eq!(u.cache_creation_tokens, 0);
    }

    #[test]
    fn parse_openai_usage_deepseek_without_miss_falls_back_to_prompt_minus_hit() {
        // If only hit is reported, fresh input falls back to prompt − hit.
        let resp = json!({
            "usage": { "prompt_tokens": 1000, "prompt_cache_hit_tokens": 700, "completion_tokens": 5 }
        });
        let u = parse_openai_usage(&resp).unwrap();
        assert_eq!(u.cache_read_tokens, 700);
        assert_eq!(u.tokens_in, 300);
    }

    #[test]
    fn inconsistent_cached_greater_than_prompt_is_observable() {
        // The decision predicate is directly testable...
        assert_eq!(split_prompt(100, 500), (0, true));

        // ...and the inconsistency is no longer silent: a targeted warn fires on this thread.
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = CaptureSubscriber {
            sink: Arc::clone(&events),
        };
        tracing::subscriber::with_default(subscriber, || {
            let resp = json!({
                "usage": { "prompt_tokens": 100, "completion_tokens": 5,
                    "prompt_tokens_details": { "cached_tokens": 500 } }
            });
            let u = parse_openai_usage(&resp).unwrap();
            assert_eq!(u.tokens_in, 0); // saturates (existing behavior preserved)
            assert_eq!(u.cache_read_tokens, 500);
        });
        let got = events.lock().unwrap();
        assert!(
            got.iter().any(|t| t == "sandhi.usage.inconsistent_cache"),
            "expected an inconsistent_cache warn, got {got:?}"
        );
    }

    #[test]
    fn apply_preserves_caller_set_measurement() {
        use crate::chat::{UsageBasis, UsageCompleteness};
        // A caller that already stamped the real outcome/completeness/attempts keeps them — apply
        // no longer overwrites with success/Final/1.
        let base = UsageEvent::new("r", "t", "openai", "o4", Backend::External)
            .with_measurement(UsageCompleteness::Partial, 3, Some("error".into()), None)
            .with_basis(UsageBasis::Estimated);
        let ev = ParsedUsage {
            tokens_in: 10,
            tokens_out: 5,
            ..Default::default()
        }
        .apply(base);
        assert_eq!(ev.outcome.as_deref(), Some("error")); // NOT overwritten with "success"
        assert_eq!(ev.usage_completeness, UsageCompleteness::Partial); // NOT overwritten with Final
        assert_eq!(ev.attempts, 3); // NOT reset to 1
        assert_eq!(ev.usage_basis, UsageBasis::Estimated); // untouched
        assert_eq!((ev.tokens_in, ev.tokens_out), (10, 5));
    }

    #[test]
    fn apply_fills_measurement_defaults_when_unset() {
        // A caller that set nothing still gets success/Final/1 (no behavior change for the
        // bindings, which rely on apply to stamp).
        let base = UsageEvent::new("r", "t", "openai", "o4", Backend::External);
        let ev = ParsedUsage {
            tokens_in: 10,
            tokens_out: 5,
            ..Default::default()
        }
        .apply(base);
        assert_eq!(ev.outcome.as_deref(), Some("success"));
        assert_eq!(ev.usage_completeness, crate::chat::UsageCompleteness::Final);
        assert_eq!(ev.attempts, 1);
    }

    #[test]
    fn cohere_usage_with_neither_subkey_returns_none() {
        // An empty/unrecognizable usage object must not fabricate a zero-token measurement.
        assert!(parse_cohere_usage(&json!({ "usage": {} })).is_none());
        assert!(parse_cohere_usage(&json!({ "usage": { "unexpected": 7 } })).is_none());
        // A shape with only output still meters (not both-zero).
        let u =
            parse_cohere_usage(&json!({ "usage": { "tokens": { "output_tokens": 9 } } })).unwrap();
        assert_eq!((u.tokens_in, u.tokens_out), (0, 9));
    }

    /// A minimal `tracing::Subscriber` that records the target of every event, so a test can
    /// assert a `tracing::warn!` fired — without `tracing-subscriber`, which sandhi-core must not
    /// depend on (TD-0011 D1: a library must not be able to install a subscriber). Spans are
    /// no-ops; only events are captured.
    struct CaptureSubscriber {
        sink: Arc<Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn event(&self, event: &tracing::Event<'_>) {
            if let Ok(mut g) = self.sink.lock() {
                g.push(event.metadata().target().to_string());
            }
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }
}
