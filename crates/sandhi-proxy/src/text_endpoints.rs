use sandhi_core::ChatRequestV1;
use serde_json::Value;

/// Count deterministic repeated context before applying the existing token estimate.
/// No repeated strings are allocated, and the same exposure is used for calibration.
pub(crate) fn input_bytes(request: &ChatRequestV1, wire_bytes: usize) -> usize {
    let read = |key: &str, default| {
        request
            .extensions
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or(default)
    };
    let expanded = (wire_bytes as u64)
        .saturating_add(read("sandhi.repeated_suffix_bytes", 0))
        .saturating_mul(read("sandhi.input_multiplier", 1));
    usize::try_from(expanded).unwrap_or(usize::MAX)
}
