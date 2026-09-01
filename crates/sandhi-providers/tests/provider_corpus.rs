//! TD-0001 W1 — usage-extraction corpus for OpenAI / Gemini / Cohere / Ollama (replay through the
//! public `Provider` API). Sibling of `anthropic_corpus.rs`.
//!
//! Recorded-fixture replay (ADR-0003 §5): serve captured responses (non-streaming JSON +
//! streamed SSE/NDJSON) through the real adapter over `wiremock`, and assert the finalized
//! `ParsedUsage` equals the per-provider `expected_usage.json`. The streaming cases also assert
//! byte-exact pass-through (O(1) forwarding, ADR-0047 D9).
//!
//! Fixtures under `tests/fixtures/<provider>/` are faithful representative captures of the
//! documented shapes; a real recording drops in unchanged.

use futures_util::StreamExt;
use sandhi_providers::{
    ByteStream, Cohere, Gemini, Ollama, OpenAiCompat, ParsedUsage, Provider, ProviderRequest,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn parse_expected(s: &str) -> ParsedUsage {
    let v: serde_json::Value = serde_json::from_str(s).unwrap();
    ParsedUsage {
        tokens_in: v["tokens_in"].as_u64().unwrap(),
        tokens_out: v["tokens_out"].as_u64().unwrap(),
        cache_creation_tokens: v["cache_creation_tokens"].as_u64().unwrap(),
        cache_read_tokens: v["cache_read_tokens"].as_u64().unwrap(),
        reasoning_tokens: 0,
    }
}

async fn drain(mut s: ByteStream) -> (Vec<u8>, ParsedUsage) {
    let mut forwarded = Vec::new();
    let mut usage = None;
    while let Some(item) = s.next().await {
        let chunk = item.unwrap();
        forwarded.extend_from_slice(&chunk.data);
        if chunk.usage.is_some() {
            usage = chunk.usage;
        }
    }
    (forwarded, usage.expect("terminal usage"))
}

async fn mock(server: &MockServer, route: &str, content_type: &str, body: &str) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", content_type)
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

// ───────────────────────── OpenAI ─────────────────────────

#[tokio::test]
async fn openai_complete_fixture_yields_expected_cache_split() {
    let server = MockServer::start().await;
    mock(
        &server,
        "/chat/completions",
        "application/json",
        include_str!("fixtures/openai/complete.json"),
    )
    .await;
    let out = OpenAiCompat::new("openai", server.uri(), "sk-test")
        .complete(ProviderRequest::new(
            "gpt-x",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(
        out.usage,
        parse_expected(include_str!("fixtures/openai/expected_usage.json"))
    );
}

#[tokio::test]
async fn openai_stream_fixture_yields_expected_and_forwards_verbatim() {
    let sse = include_str!("fixtures/openai/stream.sse");
    let server = MockServer::start().await;
    mock(&server, "/chat/completions", "text/event-stream", sse).await;
    let stream = OpenAiCompat::new("openai", server.uri(), "sk-test")
        .stream(ProviderRequest::new(
            "gpt-x",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    let (forwarded, usage) = drain(stream).await;
    assert_eq!(forwarded, sse.as_bytes());
    assert_eq!(
        usage,
        parse_expected(include_str!("fixtures/openai/expected_usage.json"))
    );
}

// ───────────────────────── Gemini ─────────────────────────

#[tokio::test]
async fn gemini_complete_fixture_yields_expected_cache_split() {
    let server = MockServer::start().await;
    mock(
        &server,
        "/models/gemini-x:generateContent",
        "application/json",
        include_str!("fixtures/gemini/complete.json"),
    )
    .await;
    let out = Gemini::new(server.uri(), "gk-test")
        .complete(ProviderRequest::new(
            "gemini-x",
            serde_json::json!({ "contents": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(
        out.usage,
        parse_expected(include_str!("fixtures/gemini/expected_usage.json"))
    );
}

#[tokio::test]
async fn gemini_stream_fixture_yields_expected_and_forwards_verbatim() {
    let sse = include_str!("fixtures/gemini/stream.sse");
    let server = MockServer::start().await;
    mock(
        &server,
        "/models/gemini-x:streamGenerateContent",
        "text/event-stream",
        sse,
    )
    .await;
    let stream = Gemini::new(server.uri(), "gk-test")
        .stream(ProviderRequest::new(
            "gemini-x",
            serde_json::json!({ "contents": [] }),
        ))
        .await
        .unwrap();
    let (forwarded, usage) = drain(stream).await;
    assert_eq!(forwarded, sse.as_bytes());
    assert_eq!(
        usage,
        parse_expected(include_str!("fixtures/gemini/expected_usage.json"))
    );
}

// ───────────────────────── Cohere ─────────────────────────

#[tokio::test]
async fn cohere_complete_fixture_yields_expected_billed_units() {
    let server = MockServer::start().await;
    mock(
        &server,
        "/v2/chat",
        "application/json",
        include_str!("fixtures/cohere/complete.json"),
    )
    .await;
    let out = Cohere::new(server.uri(), "co-test")
        .complete(ProviderRequest::new(
            "command-r",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(
        out.usage,
        parse_expected(include_str!("fixtures/cohere/expected_usage.json"))
    );
}

#[tokio::test]
async fn cohere_stream_fixture_yields_expected_and_forwards_verbatim() {
    let sse = include_str!("fixtures/cohere/stream.sse");
    let server = MockServer::start().await;
    mock(&server, "/v2/chat", "text/event-stream", sse).await;
    let stream = Cohere::new(server.uri(), "co-test")
        .stream(ProviderRequest::new(
            "command-r",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    let (forwarded, usage) = drain(stream).await;
    assert_eq!(forwarded, sse.as_bytes());
    assert_eq!(
        usage,
        parse_expected(include_str!("fixtures/cohere/expected_usage.json"))
    );
}

// ───────────────────────── Ollama (NDJSON) ─────────────────────────

#[tokio::test]
async fn ollama_complete_fixture_yields_expected_eval_counts() {
    let server = MockServer::start().await;
    mock(
        &server,
        "/api/chat",
        "application/json",
        include_str!("fixtures/ollama/complete.json"),
    )
    .await;
    let out = Ollama::new(server.uri())
        .complete(ProviderRequest::new(
            "llama3",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(
        out.usage,
        parse_expected(include_str!("fixtures/ollama/expected_usage.json"))
    );
}

#[tokio::test]
async fn ollama_stream_fixture_yields_expected_and_forwards_verbatim() {
    let ndjson = include_str!("fixtures/ollama/stream.ndjson");
    let server = MockServer::start().await;
    mock(&server, "/api/chat", "application/x-ndjson", ndjson).await;
    let stream = Ollama::new(server.uri())
        .stream(ProviderRequest::new(
            "llama3",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    let (forwarded, usage) = drain(stream).await;
    assert_eq!(forwarded, ndjson.as_bytes());
    assert_eq!(
        usage,
        parse_expected(include_str!("fixtures/ollama/expected_usage.json"))
    );
}

// ───────────────────────── InferFlux (self-hosted OpenAI dialect, ADR-0008) ─────────────────────────
//
// Real captures from a live inferfluxd v0.1.0 (tinyllama, llama_cpp backend) running with
// its prefix cache — the second identical prompt in a session reports the WHOLE prompt as
// cached (`prompt_tokens_details.cached_tokens` == `prompt_tokens`, shipped upstream after
// ADR-0008 was written). This is the regression proof of the ADR's "the split lights up
// automatically" claim: `parse_openai_usage` needed ZERO changes, and the fixture pins the
// exact neutral split — fresh input = prompt − cached = 0, cache_read = 50, billable = 74.
// InferFlux's tolerated deviations from OpenAI's streaming shape remain visible (content
// chunks carry no `"usage": null` sibling; first delta has no `role`) and the stream test
// still asserts byte-exact passthrough of the real frame, timings included (I3 rides the
// same usage frame; the typed boundary stamps its own latency separately, `typed.rs`).

#[tokio::test]
async fn inferflux_complete_fixture_meters_the_cache_split() {
    let server = MockServer::start().await;
    mock(
        &server,
        "/chat/completions",
        "application/json",
        include_str!("fixtures/inferflux/complete.json"),
    )
    .await;
    let out = OpenAiCompat::new("inferflux", server.uri(), "local-key")
        .complete(ProviderRequest::new(
            "tinyllama",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(
        out.usage,
        parse_expected(include_str!("fixtures/inferflux/expected_usage.json"))
    );
    // The split, spelled out: the whole prompt was a cache hit.
    assert_eq!(out.usage.cache_read_tokens, 50);
    assert_eq!(out.usage.tokens_in, 0);
    assert_eq!(out.usage.tokens_out, 24);
    // Billable = 0 fresh + 50 cache-read + 24 out (ADR-0005 D4 shape).
    assert_eq!(
        out.usage.tokens_in + out.usage.cache_read_tokens + out.usage.tokens_out,
        74
    );
}

#[tokio::test]
async fn inferflux_stream_fixture_yields_expected_and_forwards_verbatim() {
    let sse = include_str!("fixtures/inferflux/stream.sse");
    let server = MockServer::start().await;
    mock(&server, "/chat/completions", "text/event-stream", sse).await;
    let stream = OpenAiCompat::new("inferflux", server.uri(), "local-key")
        .stream(ProviderRequest::new(
            "tinyllama",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    let (forwarded, usage) = drain(stream).await;
    assert_eq!(forwarded, sse.as_bytes());
    assert_eq!(
        usage,
        parse_expected(include_str!("fixtures/inferflux/expected_usage.json"))
    );
    assert_eq!(usage.cache_read_tokens, 50, "the stream split meters too");
}
