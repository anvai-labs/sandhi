use sandhi_core::{ChatRequestV1, EndpointFamilyV1, NativeInputFeatureV1};
use serde_json::{json, Value};

fn native_request() -> ChatRequestV1 {
    serde_json::from_value(json!({
        "model":"fixture", "messages":[{"role":"user","content":"hello"}],
        "extensions":{"openai_responses":{"input":[
            {"type":"reasoning","encrypted_content":"opaque"},
            {"role":"user","content":"hello"}
        ]}}
    }))
    .unwrap()
}

#[test]
fn native_input_preserves_order_but_never_overrides_admitted_model_or_limit() {
    let mut request = native_request();
    request
        .capture_native_input(
            EndpointFamilyV1::OpenaiResponses,
            vec![NativeInputFeatureV1::OpaqueState],
        )
        .unwrap();
    request.model = "resolved-model".into();
    request.max_output_tokens = Some(42);
    let mut output = json!({"model":request.model,"max_output_tokens":42,"input":[]})
        .as_object()
        .unwrap()
        .clone();
    assert!(request
        .restore_native_input(EndpointFamilyV1::OpenaiResponses, &mut output)
        .unwrap());
    assert_eq!(
        output["input"],
        request.extensions["openai_responses"]["input"]
    );
    assert_eq!(output["model"], "resolved-model");
    assert_eq!(output["max_output_tokens"], 42);
    assert!(request
        .native_input_for(EndpointFamilyV1::AnthropicMessages)
        .is_err());
}

#[test]
fn changing_portable_content_or_native_payload_invalidates_the_snapshot() {
    let mut request = native_request();
    request
        .capture_native_input(EndpointFamilyV1::OpenaiResponses, vec![])
        .unwrap();
    let serialized = serde_json::to_value(&request).unwrap();
    let mut roundtrip: ChatRequestV1 = serde_json::from_value(serialized).unwrap();
    assert!(roundtrip
        .native_input_for(EndpointFamilyV1::OpenaiResponses)
        .unwrap()
        .is_some());
    roundtrip.extensions.get_mut("openai_responses").unwrap()["input"][1]["content"] =
        json!("changed");
    assert!(roundtrip.validate().is_err());
    request.messages =
        serde_json::from_value::<Vec<_>>(json!([{"role":"user","content":"changed"}])).unwrap();
    assert!(request
        .native_input_for(EndpointFamilyV1::OpenaiResponses)
        .is_err());
}

#[test]
fn portable_cross_family_requests_do_not_reuse_native_inputs() {
    let mut request = native_request();
    request.extensions.get_mut("openai_responses").unwrap()["input"] =
        json!([{"role":"user","content":"hello"}]);
    request
        .capture_native_input(EndpointFamilyV1::OpenaiResponses, vec![])
        .unwrap();
    assert!(request
        .native_input_for(EndpointFamilyV1::AnthropicMessages)
        .unwrap()
        .is_none());
    let mut output = json!({"messages":[{"role":"user","content":"hello"}]})
        .as_object()
        .unwrap()
        .clone();
    let original = output.clone();
    assert!(!request
        .restore_native_input(EndpointFamilyV1::AnthropicMessages, &mut output)
        .unwrap());
    assert_eq!(output, original);
    // It is a consistency boundary, not a credential or a bearer authority.
    let snapshot: Value = serde_json::to_value(request.native_input.unwrap()).unwrap();
    assert!(snapshot.get("credential_ref").is_none());
}
