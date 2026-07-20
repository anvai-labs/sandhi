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
