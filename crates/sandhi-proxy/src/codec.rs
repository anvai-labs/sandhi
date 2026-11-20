//! HTTP framing and credential handling for SDK dialects.

use axum::{http::HeaderMap, response::IntoResponse};
pub(crate) use sandhi_dialect::{
    decode_request, encode_response_with_emission, IngressDialect, IngressError, TranslatedStream,
};
use serde_json::Value;

pub(crate) trait HttpIngressError {
    fn render(&self, dialect: IngressDialect) -> axum::response::Response;
}

impl HttpIngressError for IngressError {
    fn render(&self, dialect: IngressDialect) -> axum::response::Response {
        let response = (self.status(), axum::Json(self.body(dialect))).into_response();
        // Deterministic projection failures must not trigger a fresh inference.
        if self.suppresses_retry() {
            crate::settlement::buffered::no_automatic_retry(response)
        } else {
            response
        }
    }
}

/// One way a client may present its virtual key on the wire.
///
/// Kept as data rather than as branches in a handler so that a dialect declares its credential
/// contract in one place (TD-0010 D1) — adding a dialect means listing its schemes, not editing
/// the auth path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialScheme {
    /// `Authorization: Bearer <vk>` — the OpenAI-family presentation.
    AuthorizationBearer,
    /// A header whose entire value is the credential, e.g. Anthropic's `x-api-key: <vk>`.
    RawHeader(&'static str),
}

impl CredentialScheme {
    fn extract(self, headers: &HeaderMap) -> Option<&str> {
        let raw = match self {
            CredentialScheme::AuthorizationBearer => headers
                .get("authorization")?
                .to_str()
                .ok()?
                .strip_prefix("Bearer ")?,
            CredentialScheme::RawHeader(name) => headers.get(name)?.to_str().ok()?,
        };
        let token = raw.trim();
        (!token.is_empty()).then_some(token)
    }
}

pub(crate) trait DialectCredentials {
    fn credential_schemes(self) -> &'static [CredentialScheme];
    fn credential_hint(self) -> &'static str;
    fn extract_credential(self, headers: &HeaderMap) -> Option<&str>;
}

impl DialectCredentials for IngressDialect {
    /// The credential presentations this dialect accepts, **most-native first**.
    ///
    /// A dialect is the whole client-facing contract, not just a body schema: a stock SDK
    /// authenticates the way its vendor documents. The Anthropic SDK sends `x-api-key`, so a
    /// gateway that only reads `Authorization: Bearer` 401s an unmodified client. `Bearer` is
    /// accepted everywhere as a secondary form — it costs nothing and it keeps curl users and
    /// header-stripping intermediaries working.
    fn credential_schemes(self) -> &'static [CredentialScheme] {
        match self {
            // No `x-api-key` here on purpose: the OpenAI SDKs never send it, so accepting it
            // would invent a cross-vendor auth scheme no client asked for.
            IngressDialect::OpenAi
            | IngressDialect::Embeddings
            | IngressDialect::Completions
            | IngressDialect::Responses => &[CredentialScheme::AuthorizationBearer],
            IngressDialect::Anthropic => &[
                CredentialScheme::RawHeader("x-api-key"),
                CredentialScheme::AuthorizationBearer,
            ],
            // Header only. Gemini also documents `?key=<credential>`, which puts a live virtual
            // key in a URL — access logs, crash reports, browser history. Until that can be
            // guaranteed not to be logged end to end, the query form is a stated gap in the
            // compatibility matrix rather than a quiet acceptance (TD-0010 D4a).
            IngressDialect::Gemini => &[
                CredentialScheme::RawHeader("x-goog-api-key"),
                CredentialScheme::AuthorizationBearer,
            ],
        }
    }

    /// How this dialect's own SDK is documented to present a credential — used in the 401 so the
    /// message tells a client what its vendor actually sends, not what another vendor sends.
    fn credential_hint(self) -> &'static str {
        match self {
            IngressDialect::OpenAi
            | IngressDialect::Embeddings
            | IngressDialect::Completions
            | IngressDialect::Responses => "'Authorization: Bearer <key>'",
            IngressDialect::Anthropic => "'x-api-key: <key>' (or 'Authorization: Bearer <key>')",
            IngressDialect::Gemini => "'x-goog-api-key: <key>' (or 'Authorization: Bearer <key>')",
        }
    }

    /// The client's virtual key, read in whichever presentation this dialect accepts.
    /// The dialect-native scheme wins when a request carries more than one.
    fn extract_credential(self, headers: &HeaderMap) -> Option<&str> {
        self.credential_schemes()
            .iter()
            .find_map(|scheme| scheme.extract(headers))
    }
}

/// Strip the gateway-reserved `sandhi_extensions` key from forwarded request
/// bytes (ADR-0013): the key is gateway-consumed and must never reach an
/// upstream on any plane. Rewrites the bytes only when the key is actually
/// present at the top level, so ordinary traffic stays byte-exact — a mention
/// inside message content parses to a nested position and leaves the bytes
/// untouched, and non-JSON or non-object bodies pass through unchanged.
pub(crate) fn strip_reserved_passthrough(body: bytes::Bytes) -> bytes::Bytes {
    const RESERVED: &[&[u8]] = &[b"sandhi_extensions", b"sandhi_cache"];
    // Fast path: the literal bytes are absent AND no JSON unicode escape is
    // present, so no spelling of the reserved key can exist — stay byte-exact
    // without a parse. Any non-literal spelling of an ASCII key requires at
    // least one `\uXXXX` escape, so checking for `\u` is a necessary condition
    // (bodies carrying escaped non-ASCII text simply pay one parse).
    let mentions_reserved = RESERVED
        .iter()
        .any(|key| body.windows(key.len()).any(|window| window == *key));
    let mentions_escape = body.windows(2).any(|w| w == b"\\u");
    if !(mentions_reserved || mentions_escape) {
        return body;
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(object) = value.as_object_mut() else {
        return body;
    };
    let removed_extensions = object.remove("sandhi_extensions").is_some();
    let removed_cache = object.remove("sandhi_cache").is_some();
    if removed_extensions || removed_cache {
        return bytes::Bytes::from(serde_json::to_vec(&value).expect("re-serializing parsed JSON"));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use serde_json::json;

    #[tokio::test]
    async fn typed_nonretryable_errors_suppress_status_based_sdk_retries_in_every_dialect() {
        for dialect in [
            IngressDialect::OpenAi,
            IngressDialect::Anthropic,
            IngressDialect::Responses,
            IngressDialect::Gemini,
        ] {
            for full in [false, true] {
                for error in [
                    sandhi_providers::ProviderError::OutputProjection(
                        "synthetic opaque state".into(),
                    ),
                    sandhi_providers::ProviderError::Auth,
                ] {
                    let error = if full {
                        IngressError::from_provider_full(&error, "fixture")
                    } else {
                        IngressError::from_provider_redacted(&error, "fixture")
                    };
                    let response = error.render(dialect);
                    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                    assert_eq!(response.headers()["x-should-retry"], "false");
                    let bytes = axum::body::to_bytes(response.into_body(), 10_000)
                        .await
                        .unwrap();
                    if !full {
                        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic opaque state"));
                    }
                }
            }
            for error in [
                sandhi_providers::ProviderError::Transport("transient".into()),
                sandhi_providers::ProviderError::RateLimited,
            ] {
                let response =
                    IngressError::from_provider_redacted(&error, "fixture").render(dialect);
                assert!(!response.headers().contains_key("x-should-retry"));
            }
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn strip_reserved_passthrough_rewrites_only_when_the_key_is_top_level() {
        use bytes::Bytes;

        let with_key = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "gpt-5",
                "messages": [{"role": "user", "content": "hi"}],
                "sandhi_extensions": {"anthropic": {"top_k": 5}}
            }))
            .unwrap(),
        );
        let stripped = strip_reserved_passthrough(with_key.clone());
        assert_ne!(stripped, with_key, "the reserved key must be removed");
        let value: Value = serde_json::from_slice(&stripped).unwrap();
        assert!(value.get("sandhi_extensions").is_none());
        assert_eq!(value["model"], "gpt-5");

        // Absent key and mention inside message content: byte-identical.
        let plain = Bytes::from_static(br#"{"model":"gpt-5","messages":[]}"#);
        assert_eq!(strip_reserved_passthrough(plain.clone()), plain);
        let nested = Bytes::from(
            serde_json::to_vec(&json!({
                "model":"gpt-5",
                "messages":[{"role":"user","content":"sandhi_extensions is not a top-level key"}]
            }))
            .unwrap(),
        );
        assert_eq!(strip_reserved_passthrough(nested.clone()), nested);

        // Non-object JSON containing the literal: unchanged.
        let array = Bytes::from_static(br#"["sandhi_extensions"]"#);
        assert_eq!(strip_reserved_passthrough(array.clone()), array);

        // A unicode-escaped spelling of the reserved key (hand-crafted; no
        // standard serializer escapes ASCII) still parses to the same key and
        // must be stripped — the `\u` presence check drives the parse.
        let escaped = Bytes::from_static(
            concat!(
                r#"{"model":"gpt-5","#,
                "\"\\u0073andhi_extensions\":{\"a\":1}}"
            )
            .as_bytes(),
        );
        let stripped = strip_reserved_passthrough(escaped.clone());
        assert_ne!(stripped, escaped, "escaped spelling must not survive");
        let value: Value = serde_json::from_slice(&stripped).unwrap();
        assert!(value
            .as_object()
            .unwrap()
            .get("sandhi_extensions")
            .is_none());

        // Escaped non-ASCII elsewhere with no reserved key: byte-identical
        // (the parse finds nothing to remove).
        let escaped_text = Bytes::from_static(
            "{\"model\":\"gpt-5\",\"messages\":[{\"role\":\"user\",\"content\":\"\u{4f60}\u{597d}\"}]}".as_bytes(),
        );
        assert_eq!(
            strip_reserved_passthrough(escaped_text.clone()),
            escaped_text
        );
    }

    #[test]
    fn anthropic_dialect_accepts_the_sdks_x_api_key_header() {
        // The stock Anthropic SDK authenticates with `x-api-key` and nothing else.
        assert_eq!(
            IngressDialect::Anthropic.extract_credential(&headers(&[("x-api-key", "vk_demo")])),
            Some("vk_demo")
        );
    }

    #[test]
    fn anthropic_dialect_still_accepts_bearer_and_prefers_the_native_header() {
        assert_eq!(
            IngressDialect::Anthropic
                .extract_credential(&headers(&[("authorization", "Bearer vk_bearer")])),
            Some("vk_bearer")
        );
        assert_eq!(
            IngressDialect::Anthropic.extract_credential(&headers(&[
                ("authorization", "Bearer vk_bearer"),
                ("x-api-key", "vk_native"),
            ])),
            Some("vk_native")
        );
    }

    #[test]
    fn openai_dialects_accept_bearer_only() {
        for dialect in [IngressDialect::OpenAi, IngressDialect::Responses] {
            assert_eq!(
                dialect.extract_credential(&headers(&[("authorization", "Bearer vk_demo")])),
                Some("vk_demo")
            );
            // `x-api-key` is not an OpenAI credential scheme — accepting it would invent one.
            assert_eq!(
                dialect.extract_credential(&headers(&[("x-api-key", "vk_demo")])),
                None
            );
        }
    }

    #[test]
    fn missing_or_malformed_credentials_extract_to_none() {
        for dialect in [
            IngressDialect::OpenAi,
            IngressDialect::Responses,
            IngressDialect::Anthropic,
        ] {
            assert_eq!(dialect.extract_credential(&HeaderMap::new()), None);
            // Wrong scheme, empty token, and an empty native header all fail closed.
            assert_eq!(
                dialect.extract_credential(&headers(&[("authorization", "Basic vk_demo")])),
                None
            );
            assert_eq!(
                dialect.extract_credential(&headers(&[("authorization", "Bearer   ")])),
                None
            );
            assert_eq!(
                dialect.extract_credential(&headers(&[
                    ("x-api-key", "   "),
                    ("authorization", "Bearer vk_demo"),
                ])),
                Some("vk_demo")
            );
        }
    }
}
