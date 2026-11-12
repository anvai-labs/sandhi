//! Synthetic HTTP qualification, not a live subscription acceptance claim.
use sandhi_core::{ChatRequestV1, UsageCompleteness};
use sandhi_providers::{
    AttemptContext, AttemptOutcome, AttemptPhase, ProviderError, ProviderRuntime,
};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn request(native: bool) -> ChatRequestV1 {
    serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,"include_native_response":native,
        "messages":[{"role":"developer","content":"Write code."},{"role":"user","content":"hello"}]})).unwrap()
}

fn event(kind: &str, response: Value) -> String {
    format!(
        "event: {kind}\ndata: {}\n\n",
        json!({"type":kind,"response":response})
    )
}

fn prefix() -> String {
    format!(
        "{}data: {{\"type\":\"response.output_text.delta\",\"delta\":\"reply\"}}\n\n",
        event(
            "response.in_progress",
            json!({"id":"resp_fixture","usage":{"input_tokens":10,"output_tokens":1,"input_tokens_details":{"cached_tokens":6}}})
        )
    )
}

fn terminal(status: &str) -> Value {
    json!({"id":"resp_fixture","status":status,"model":"fixture","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply","annotations":[]}]},{"type":"reasoning","encrypted_content":"opaque-fixture","summary":[]}],
        "usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14,
        "input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}}})
}

#[tokio::test]
async fn codex_qualified_aggregation_retains_source_usage_without_raw_bypass_or_replay() {
    for (opaque, native) in [(false, false), (false, true), (true, false), (true, true)] {
        for mode in [
            "final",
            "incomplete",
            "failed",
            "missing",
            "malformed",
            "progress_only",
            "post_terminal",
            "truncated",
            "duplicate",
            "identity",
            "multiline",
            "unframed_terminal",
        ] {
            let server = MockServer::start().await;
            let status = match mode {
                "incomplete" => "incomplete",
                "failed" => "failed",
                _ => "completed",
            };
            let mut body = terminal(status);
            if status == "incomplete" {
                body["incomplete_details"] = json!({"reason":"max_output_tokens"});
            }
            if !opaque {
                body["output"] = json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply","annotations":[]}]}]);
            }
            if mode == "missing" {
                body.as_object_mut().unwrap().remove("usage");
            }
            if mode == "malformed" {
                body["usage"]["output_tokens"] = json!("bad");
            }
            if mode == "identity" {
                body["id"] = json!("other");
            }
            let mut wire = prefix();
            if mode != "progress_only" {
                wire.push_str(&event(&format!("response.{status}"), body.clone()));
            }
            if mode == "post_terminal" {
                wire.push_str("data: {}\n\n");
            }
            if mode == "truncated" {
                wire.push_str("data: {");
            }
            if mode == "duplicate" {
                wire.push_str(&event("response.completed", body));
            }
            if mode == "unframed_terminal" {
                wire.truncate(wire.len() - 2);
            }
            if mode == "multiline" {
                let mut framed = String::from("\u{feff}");
                for stanza in wire.split("\n\n") {
                    if let Some(data) = stanza.lines().find_map(|line| line.strip_prefix("data: "))
                    {
                        let value: Value = serde_json::from_str(data).unwrap();
                        for line in serde_json::to_string_pretty(&value).unwrap().lines() {
                            framed.push_str(&format!("data: {line}\n"));
                        }
                        framed.push('\n');
                    }
                }
                wire = framed.replace('\n', "\r\n");
            }
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(wire, "text/event-stream")
                        .insert_header("x-request-id", "codex-origin"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "chatgpt-account-id",
                http::HeaderValue::from_static("account-fixture"),
            );
            let handle = ProviderRuntime::new()
                .chatgpt_responses(
                    "models",
                    server.uri(),
                    "synthetic-access",
                    headers,
                    Some(0),
                    None,
                    None,
                    None,
                )
                .with_credential_expiry(4_000_000_000);
            assert!(handle.raw_forwarder().is_none());
            assert!(handle.supports_owned_buffered());
            handle.preflight_owned_buffered(&request(native)).unwrap();
            let (context, receiver) =
                AttemptContext::single_attempt("execution", "a".repeat(64)).unwrap();
            let mut spoof = http::HeaderMap::new();
            spoof.insert(
                "chatgpt-account-id",
                http::HeaderValue::from_static("spoof"),
            );
            spoof.insert(
                "authorization",
                http::HeaderValue::from_static("Bearer spoof"),
            );
            let result = handle
                .complete_with_qualified_usage_and_attempts(request(native), spoof, context.clone())
                .await;
            let good = matches!(mode, "final" | "incomplete" | "multiline");
            if good && (native || !opaque) {
                let result = result.unwrap();
                let usage = result.usage.unwrap();
                assert_eq!(
                    (
                        usage.tokens_in,
                        usage.cache_read_tokens,
                        usage.cache_creation_tokens,
                        usage.tokens_out
                    ),
                    (2, 6, 2, 4)
                );
                assert_eq!(usage.reasoning_tokens, Some(0));
                assert_eq!(usage.upstream_request_id.as_deref(), Some("codex-origin"));
                assert_eq!(
                    result.response.extensions.contains_key("openai_responses"),
                    native
                );
                if native && opaque {
                    assert_eq!(
                        result.response.extensions["openai_responses"]["output"][1]
                            ["encrypted_content"],
                        "opaque-fixture"
                    );
                }
            } else if good {
                assert!(result.is_err(), "opaque reasoning became portable: {mode}");
            } else if let Ok(result) = result {
                assert!(result.usage.is_err(), "{mode}");
            }
            let evidence = context.qualified_stream_evidence().unwrap();
            let usage = evidence.usage.unwrap();
            assert_eq!(
                usage.completeness,
                if good || mode == "failed" {
                    UsageCompleteness::Final
                } else {
                    UsageCompleteness::Partial
                },
                "{mode}"
            );
            assert_eq!(usage.cache_read_tokens, 6);
            assert_eq!(
                usage.tokens_out,
                if matches!(
                    mode,
                    "progress_only" | "missing" | "malformed" | "identity" | "unframed_terminal"
                ) {
                    1
                } else {
                    4
                },
                "{mode}"
            );
            assert_eq!(usage.upstream_request_id.as_deref(), Some("codex-origin"));
            let observations = receiver.drain();
            assert_eq!(context.dispatch_count(), 1);
            assert_eq!(observations.len(), 2, "{mode}");
            let expected = if mode == "failed" {
                AttemptOutcome::ProviderRejected
            } else if good {
                AttemptOutcome::Success
            } else {
                AttemptOutcome::IncompleteStream
            };
            assert!(
                matches!(&observations[1].phase,AttemptPhase::Terminal{outcome,provider_status:Some(200),..} if *outcome==expected),
                "{mode}"
            );
            assert!(handle
                .complete_with_qualified_usage_and_attempts(
                    request(native),
                    Default::default(),
                    context
                )
                .await
                .is_err());
            let calls = server.received_requests().await.unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].url.path(), "/responses");
            assert_eq!(calls[0].headers["authorization"], "Bearer synthetic-access");
            assert_eq!(calls[0].headers["chatgpt-account-id"], "account-fixture");
            let sent: Value = calls[0].body_json().unwrap();
            assert_eq!(sent["stream"], true);
            assert_eq!(sent["store"], false);
            assert_eq!(sent["instructions"], "Write code.");
            assert!(sent["input"].is_array());
            assert!(sent.get("max_output_tokens").is_none());
        }
    }
}

#[tokio::test]
async fn qualified_codex_retries_and_expired_leases_refuse_before_http() {
    let server = MockServer::start().await;
    for (retries, expiry) in [(1, 4_000_000_000), (0, 1)] {
        let handle = ProviderRuntime::new()
            .chatgpt_responses(
                "models",
                server.uri(),
                "synthetic-access",
                Default::default(),
                Some(retries),
                None,
                None,
                None,
            )
            .with_credential_expiry(expiry);
        let (context, receiver) =
            AttemptContext::single_attempt("execution", "b".repeat(64)).unwrap();
        assert!(handle.preflight_owned_buffered(&request(false)).is_err());
        let result = handle
            .complete_with_qualified_usage_and_attempts(
                request(false),
                Default::default(),
                context.clone(),
            )
            .await;
        assert!(matches!(
            result,
            Err(ProviderError::InvalidRequest(_) | ProviderError::Auth)
        ));
        assert!(receiver.drain().is_empty());
        assert!(context.qualified_stream_evidence().is_none());
        assert_eq!(context.dispatch_count(), 0);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
