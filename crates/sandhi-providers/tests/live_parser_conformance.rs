//! LIVE parser-conformance — the **drift detector**.
//!
//! `provider_corpus.rs` / `anthropic_corpus.rs` replay *captured* responses over a mock: they
//! pin known usage shapes (a regression defense) but cannot tell when a provider *changes* its
//! usage JSON. This file makes one real, tiny call to each provider whose key is in the
//! environment and asserts the parser still extracts sane usage — so a renamed or moved token
//! field is caught before users see wrong metering.
//!
//! It is `#[ignore]`d (normal CI is unaffected) and a no-op without keys. Run it manually, or
//! from a secrets-bearing scheduled job:
//!
//! ```text
//! OPENAI_API_KEY=… ANTHROPIC_API_KEY=… GEMINI_API_KEY=… \
//!     cargo test -p sandhi-providers --test live_parser_conformance -- --ignored --nocapture
//! ```
//!
//! Override the model or point at a gateway with `OPENAI_MODEL` / `OPENAI_BASE` (and likewise
//! for Anthropic / Gemini). A missing key skips that provider rather than failing the run.

use std::env;

use sandhi_providers::{Anthropic, Gemini, OpenAiCompat, ParsedUsage, Provider, ProviderRequest};
use serde_json::json;

/// A real chat call must report non-zero input AND output, and no dimension above the plausible
/// per-call ceiling. A parser that silently zeroes a renamed field (`tokens_in == 0` or
/// `tokens_out == 0`) or reads a wrong huge field fails here — the drift signal.
const MAX_PLAUSIBLE_TOKENS: u64 = 50_000_000_000;

fn assert_sane(provider: &str, usage: &ParsedUsage) {
    eprintln!("{provider} live usage: {usage:?}");
    assert!(
        usage.tokens_in > 0,
        "{provider}: tokens_in == 0 — the provider's input-token field may have drifted"
    );
    assert!(
        usage.tokens_out > 0,
        "{provider}: tokens_out == 0 — the provider's output-token field may have drifted"
    );
    for (name, val) in [
        ("tokens_in", usage.tokens_in),
        ("tokens_out", usage.tokens_out),
        ("cache_creation_tokens", usage.cache_creation_tokens),
        ("cache_read_tokens", usage.cache_read_tokens),
        ("reasoning_tokens", usage.reasoning_tokens),
    ] {
        assert!(
            val <= MAX_PLAUSIBLE_TOKENS,
            "{provider}: {name}={val} exceeds the plausible ceiling — a wrong field may be being read"
        );
    }
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.into())
}

fn key(name: &str) -> Option<String> {
    env::var(name).ok().filter(|s| !s.is_empty())
}

#[ignore]
#[tokio::test]
async fn live_openai_usage_is_sane() {
    let Some(key) = key("OPENAI_API_KEY") else {
        eprintln!("openai: skipped (OPENAI_API_KEY unset)");
        return;
    };
    let base = env_or("OPENAI_BASE", "https://api.openai.com/v1");
    let model = env_or("OPENAI_MODEL", "gpt-4o-mini");
    let body = json!({
        "model": model.as_str(),
        "messages": [{"role": "user", "content": "Reply with exactly one word: ok"}],
        "max_tokens": 5,
        "temperature": 0,
    });
    let out = OpenAiCompat::new("openai", base, key)
        .complete(ProviderRequest::new(model, body))
        .await
        .unwrap_or_else(|e| panic!("openai live call failed (auth, network, or model?): {e:?}"));
    assert_sane("openai", &out.usage);
}

#[ignore]
#[tokio::test]
async fn live_anthropic_usage_is_sane() {
    let Some(key) = key("ANTHROPIC_API_KEY") else {
        eprintln!("anthropic: skipped (ANTHROPIC_API_KEY unset)");
        return;
    };
    let base = env_or("ANTHROPIC_BASE", "https://api.anthropic.com");
    let model = env_or("ANTHROPIC_MODEL", "claude-3-5-haiku-latest");
    let body = json!({
        "model": model.as_str(),
        "messages": [{"role": "user", "content": "Reply with exactly one word: ok"}],
        "max_tokens": 5,
    });
    let out = Anthropic::new(base, key)
        .complete(ProviderRequest::new(model, body))
        .await
        .unwrap_or_else(|e| panic!("anthropic live call failed (auth, network, or model?): {e:?}"));
    assert_sane("anthropic", &out.usage);
}

#[ignore]
#[tokio::test]
async fn live_gemini_usage_is_sane() {
    let Some(key) = key("GEMINI_API_KEY") else {
        eprintln!("gemini: skipped (GEMINI_API_KEY unset)");
        return;
    };
    let base = env_or(
        "GEMINI_BASE",
        "https://generativelanguage.googleapis.com/v1beta",
    );
    let model = env_or("GEMINI_MODEL", "gemini-1.5-flash");
    // Gemini takes the model in the URL path (`/models/{model}:generateContent`), not the body.
    let body = json!({
        "contents": [{"parts": [{"text": "Reply with exactly one word: ok"}]}],
        "generationConfig": {"maxOutputTokens": 5, "temperature": 0.0},
    });
    let out = Gemini::new(base, key)
        .complete(ProviderRequest::new(model, body))
        .await
        .unwrap_or_else(|e| panic!("gemini live call failed (auth, network, or model?): {e:?}"));
    assert_sane("gemini", &out.usage);
}
