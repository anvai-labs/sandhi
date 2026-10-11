//! Shared credential comparison and HTTP mutation admission.
//!
//! No route registration, provider lookup, or mutable proxy state. Callers must
//! authenticate before requesting a lifecycle guard and retain it through publication.
use crate::lifecycle::{Lifecycle, OperationGuard};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;

/// Constant-time byte comparison for the admin token (ADR-0004 D4): every byte is visited
/// regardless of where the first mismatch is, so response timing does not leak a prefix-match
/// oracle. Delegates to `subtle::ConstantTimeEq`, which carries optimizer barriers a
/// hand-rolled `|=` loop lacks (LLVM is permitted to short-circuit one). `ct_eq` on slices
/// returns early on a length mismatch — acceptable, the token length is not a secret.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

pub(crate) fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn draining_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("retry-after", "1")],
        Json(json!({"error": "Gateway is draining; mutation not admitted", "code": "gateway_draining"})),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
pub(crate) fn admit_mutation(lifecycle: &Arc<Lifecycle>) -> Result<OperationGuard, Response> {
    lifecycle.try_operation().ok_or_else(draining_response)
}

#[cfg(test)]
mod ct_tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_equality_semantics() {
        assert!(constant_time_eq(b"admin-secret", b"admin-secret"));
        assert!(constant_time_eq(b"", b""));
        // Same length, differing at the first / middle / last byte.
        assert!(!constant_time_eq(b"admin-secret", b"bdmin-secret"));
        assert!(!constant_time_eq(b"admin-secret", b"admin+secret"));
        assert!(!constant_time_eq(b"admin-secret", b"admin-secreT"));
        // Length mismatches, including prefix relationships.
        assert!(!constant_time_eq(b"admin", b"admin-secret"));
        assert!(!constant_time_eq(b"admin-secret", b"admin"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
