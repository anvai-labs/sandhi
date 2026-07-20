//! TD-0001 W1 — Anthropic usage-extraction corpus (replay through the public `Provider` API).
//!
//! Recorded-fixture replay (ADR-0003 §5): serve captured Anthropic Messages responses
//! (non-streaming JSON + streamed SSE, both with the prompt-cache split non-zero) through the
//! real `Anthropic` adapter over `wiremock`, and assert the finalized `ParsedUsage` equals the
//! `expected_usage.json` ground truth. The streaming case additionally asserts byte-exact
//! pass-through (O(1) forwarding, ADR-0047 D9).
//!
//! The fixtures under `tests/fixtures/anthropic/` are faithful representative captures of the
//! documented Messages streaming/non-streaming shapes; a real recording drops in unchanged.

use futures_util::StreamExt;
use sandhi_providers::{Anthropic, ParsedUsage, Provider, ProviderRequest};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const COMPLETE_JSON: &str = include_str!("fixtures/anthropic/complete_cache_split.json");
const STREAM_SSE: &str = include_str!("fixtures/anthropic/stream_cache_split.sse");

fn expected() -> ParsedUsage {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/anthropic/expected_usage.json")).unwrap();
    ParsedUsage {
        tokens_in: v["tokens_in"].as_u64().unwrap(),
        tokens_out: v["tokens_out"].as_u64().unwrap(),
        cache_creation_tokens: v["cache_creation_tokens"].as_u64().unwrap(),
        cache_read_tokens: v["cache_read_tokens"].as_u64().unwrap(),
    }
}

#[tokio::test]
async fn complete_fixture_yields_expected_cache_split() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(COMPLETE_JSON),
        )
        .mount(&server)
        .await;

    let out = Anthropic::new(server.uri(), "ak-test")
        .complete(ProviderRequest::new(
            "claude-x",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();

    assert_eq!(out.usage, expected());
}

#[tokio::test]
async fn stream_fixture_yields_expected_usage_and_forwards_bytes_verbatim() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(STREAM_SSE),
        )
        .mount(&server)
        .await;

    let mut stream = Anthropic::new(server.uri(), "ak-test")
        .stream(ProviderRequest::new(
            "claude-x",
            serde_json::json!({ "messages": [] }),
        ))
        .await
        .unwrap();

    let mut forwarded: Vec<u8> = Vec::new();
    let mut final_usage: Option<ParsedUsage> = None;
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        forwarded.extend_from_slice(&chunk.data);
        if chunk.usage.is_some() {
            final_usage = chunk.usage;
        }
    }

    // Byte-exact pass-through: the caller sees the upstream SSE unchanged.
    assert_eq!(forwarded, STREAM_SSE.as_bytes());
    assert_eq!(final_usage.unwrap(), expected());
}
