//! Explicit immediate-gateway contract; never infer this role from headers or credentials.
use crate::ProviderError;
use sandhi_core::{EndpointFamilyV1, PeerRoleV1, UsageConventionV1, USAGE_CONVENTION_HEADER};

pub(crate) fn resolve(
    role: PeerRoleV1,
    family: EndpointFamilyV1,
    headers: &http::HeaderMap,
) -> Result<Option<UsageConventionV1>, ProviderError> {
    if role == PeerRoleV1::Provider {
        return Ok(None);
    }
    let mut values = headers.get_all(USAGE_CONVENTION_HEADER).iter();
    let value = values
        .next()
        .filter(|_| values.next().is_none())
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= 64)
        .and_then(|value| value.parse::<UsageConventionV1>().ok())
        .filter(|convention| convention.family() == family)
        .ok_or_else(|| {
            ProviderError::OutputProjection("missing or invalid gateway usage convention".into())
        })?;
    Ok(Some(value))
}

pub(crate) fn request_headers(
    role: PeerRoleV1,
    headers: &http::HeaderMap,
    request: &crate::ProviderRequest,
) -> http::HeaderMap {
    let mut headers = crate::merge_call_headers(headers, &request.extra_headers);
    if role == PeerRoleV1::SandhiGatewayV1 {
        // Local logical-meter dedup is not gateway execution identity or replay authority.
        headers.remove("idempotency-key");
        headers.remove("chatgpt-account-id");
        headers.remove(USAGE_CONVENTION_HEADER);
        if let Some(session) = request
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Ok(value) =
                http::HeaderValue::from_str(&sandhi_core::sanitize_affinity_value(session))
            {
                if !headers.contains_key("x-sandhi-session") {
                    headers.insert("x-sandhi-session", value);
                }
            }
        }
    }
    headers
}

pub(crate) fn valid_gateway_base(base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base) else {
        return false;
    };
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (url.scheme() == "https" || url.scheme() == "http" && loopback)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_requires_explicit_peer_exact_version_family_and_one_value() {
        for value in ["v1-chat-standard", "v1-chat-terminal"] {
            let mut headers = http::HeaderMap::new();
            headers.insert(USAGE_CONVENTION_HEADER, value.parse().unwrap());
            assert!(resolve(
                PeerRoleV1::SandhiGatewayV1,
                EndpointFamilyV1::OpenaiChatCompletions,
                &headers
            )
            .unwrap()
            .is_some());
            assert!(resolve(
                PeerRoleV1::Provider,
                EndpointFamilyV1::AnthropicMessages,
                &headers
            )
            .unwrap()
            .is_none());
            assert!(resolve(
                PeerRoleV1::SandhiGatewayV1,
                EndpointFamilyV1::AnthropicMessages,
                &headers
            )
            .is_err());
            headers.append(USAGE_CONVENTION_HEADER, value.parse().unwrap());
            assert!(resolve(
                PeerRoleV1::SandhiGatewayV1,
                EndpointFamilyV1::OpenaiChatCompletions,
                &headers
            )
            .is_err());
        }
        for value in [
            "",
            "chatgpt_codex",
            "v2-chat-standard",
            "v1-chat-standard, v1-chat-terminal",
            "v1-messages-implicit-cache ",
        ] {
            let mut headers = http::HeaderMap::new();
            headers.insert(USAGE_CONVENTION_HEADER, value.parse().unwrap());
            assert!(resolve(
                PeerRoleV1::SandhiGatewayV1,
                EndpointFamilyV1::OpenaiChatCompletions,
                &headers
            )
            .is_err());
        }
        assert!(resolve(
            PeerRoleV1::SandhiGatewayV1,
            EndpointFamilyV1::OpenaiChatCompletions,
            &http::HeaderMap::new()
        )
        .is_err());
    }
    #[test]
    fn gateway_origin_is_explicit_and_does_not_embed_credentials() {
        for url in [
            "https://gateway.example/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:1234/v1",
            "http://localhost/v1",
        ] {
            assert!(valid_gateway_base(url), "{url}");
        }
        for url in [
            "http://gateway.example/v1",
            "https://user:password@gateway.example/v1",
            "https://gateway.example/v1?key=synthetic",
            "file:///tmp/gateway",
            "https://gateway.example/v1#fragment",
        ] {
            assert!(!valid_gateway_base(url), "{url}");
        }
    }
}

/// Bounded diagnostic facts from an explicitly configured peer. No usage,
/// credential, policy capability, raw body or replay authorization lives here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayFailure {
    pub code: String,
    /// Bounded display-only context; never parsed for policy or replay decisions.
    pub message: Option<String>,
    pub http_status: Option<u16>,
    pub request_id: Option<String>,
}
impl GatewayFailure {
    pub(crate) fn into_error(self) -> ProviderError {
        ProviderError::GatewayFailure(self)
    }
}

fn label(value: &str, cap: usize) -> Option<String> {
    (!value.is_empty() && value.len() <= cap && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}
fn code(value: &str) -> Option<String> {
    (value.len() <= 96
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
    .then(|| value.to_owned())
}
pub(crate) fn request_id(headers: &http::HeaderMap) -> Option<String> {
    ["x-sandhi-request-id", "x-request-id", "request-id"]
        .into_iter()
        .find_map(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| label(value, 256))
        })
}
pub(crate) fn failure_from_value(
    error: Option<&serde_json::Value>,
    status: Option<u16>,
    id: Option<String>,
) -> GatewayFailure {
    GatewayFailure {
        message: error
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| label(value, 512)),
        code: error
            .and_then(|error| error.get("code"))
            .and_then(serde_json::Value::as_str)
            .and_then(code)
            .unwrap_or_else(|| {
                match status {
                    Some(401 | 403) => "authentication_error",
                    Some(429) => "rate_limited",
                    _ => "gateway_error",
                }
                .into()
            }),
        http_status: status.or_else(|| {
            error
                .and_then(|error| error.get("http_status").or_else(|| error.get("code")))
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| (400..=599).contains(value))
        }),
        request_id: id.or_else(|| {
            error
                .and_then(|error| error.get("request_id"))
                .and_then(serde_json::Value::as_str)
                .and_then(|value| label(value, 256))
        }),
    }
}

/// Generic gateway errors are ambiguous execution outcomes. A status or prose
/// message cannot authorize inference repair; direct-source behavior is unchanged.
pub(crate) fn terminal_error(role: PeerRoleV1, error: ProviderError) -> ProviderError {
    if role == PeerRoleV1::Provider || !error.as_typed(None).retryable {
        return error;
    }
    let typed = error.as_typed(None);
    GatewayFailure {
        message: label(&typed.message, 512),
        code: typed.code,
        http_status: typed.http_status,
        request_id: typed.request_id,
    }
    .into_error()
}

pub(crate) fn stream_error(
    error: ProviderError,
    observation: &crate::raw::QualifiedStreamObservation,
) -> ProviderError {
    let evidence = observation.evidence();
    if let Some(failure) = evidence.gateway_failure {
        return failure.into_error();
    }
    terminal_error(
        if evidence.gateway_peer {
            PeerRoleV1::SandhiGatewayV1
        } else {
            PeerRoleV1::Provider
        },
        error,
    )
}

pub(crate) async fn error_for_response(
    role: PeerRoleV1,
    mut response: reqwest::Response,
    vendor_header: Option<&str>,
) -> ProviderError {
    if role == PeerRoleV1::Provider {
        return crate::error_for_response(response, vendor_header).await;
    }
    let status = response.status().as_u16();
    let id = request_id(response.headers());
    let mut bytes = Vec::new();
    // Never read an unbounded error response. A truncated/malformed body cannot
    // provide typed facts, but the terminal no-replay result remains conservative.
    while let Ok(Some(chunk)) = response.chunk().await {
        let remaining = crate::UPSTREAM_ERROR_BODY_CAP.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining || bytes.len() == crate::UPSTREAM_ERROR_BODY_CAP {
            break;
        }
    }
    let body = sandhi_core::policy::strict_json(&bytes).ok();
    failure_from_value(
        body.as_ref().and_then(|body| body.get("error")),
        Some(status),
        id,
    )
    .into_error()
}

pub(crate) fn response_request_id(
    role: PeerRoleV1,
    headers: &http::HeaderMap,
    vendor: Option<&str>,
) -> Option<String> {
    if role == PeerRoleV1::SandhiGatewayV1 {
        request_id(headers)
    } else {
        crate::provider_request_id(headers, vendor)
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[test]
    fn bounded_error_context_is_display_only_and_cannot_authorize_replay() {
        let body = serde_json::json!({"code":"output_projection_error","message":"operator-readable context","retryable":true,"http_status":409,"request_id":"body-id"});
        let failure = failure_from_value(Some(&body), Some(502), Some("http-id".into()));
        assert_eq!(failure.http_status, Some(502));
        assert_eq!(failure.request_id.as_deref(), Some("http-id"));
        let typed = failure.into_error().as_typed(None);
        assert_eq!(typed.code, "output_projection_error");
        assert!(!typed.retryable);
        assert!(typed.message.contains("operator-readable context"));
        let body = serde_json::json!({"code":"bad\ncode","message":"terminal\u{001b}[31m","request_id":"x".repeat(257)});
        let failure = failure_from_value(Some(&body), Some(502), None);
        assert_eq!(failure.code, "gateway_error");
        assert!(failure.message.is_none());
        assert!(failure.request_id.is_none());
    }
}
