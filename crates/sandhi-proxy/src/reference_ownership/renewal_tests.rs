use super::*;
use sandhi_core::{InMemorySink, KeyStore, RequestMetadataV1};
use sandhi_providers::{BackendBinding, ProviderRuntime};
use serde_json::json;
fn codex(
    token: &str,
    account: &str,
    url: &str,
    generation: u64,
    expiry: u64,
    extra_header: bool,
) -> ProviderHandle {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("chatgpt-account-id", account.parse().unwrap());
    if extra_header {
        headers.insert("x-static-context", "changed".parse().unwrap());
    }
    ProviderRuntime::new()
        .chatgpt_responses("vendor", url, token, headers, Some(0), None, None, None)
        .with_credential_expiry(expiry)
        .with_backend_binding(BackendBinding {
            id: "backend".into(),
            revision: 1,
            provider: "vendor".into(),
            credential_ref: "openai:lease".into(),
            credential_generation: generation,
            endpoint_family: Family::OpenaiResponses,
            endpoint_profile: "chatgpt_codex".into(),
            models: vec![
                serde_json::from_value(json!({"id":"model"})).unwrap(),
                serde_json::from_value(json!({"id":"other-model"})).unwrap(),
            ],
            cache_policy: None,
        })
}
fn request(model: &str) -> ChatRequestV1 {
    crate::codec::decode_request(IngressDialect::Responses,json!({"model":model,"instructions":"Answer.","input":[{"type":"reasoning","id":"rs_issued","summary":[],"encrypted_content":"source_opaque"},{"role":"user","content":"continue"}]}),RequestMetadataV1::default()).unwrap().0
}
#[tokio::test]
async fn same_account_fenced_renewal_continues_after_real_old_lease_expiry_but_every_current_lease_is_checked(
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let state = Arc::new(ProxyState::new(
        KeyStore::new(),
        crate::ProxyLedger::durable(path.to_str().unwrap(), 1).unwrap(),
        Arc::new(InMemorySink::new()),
        Default::default(),
        None,
    ));
    crate::initialize_execution(&state).unwrap();
    let clock = now();
    let old_expiry = clock + 3;
    let admission_clock = clock - 31;
    let old = codex(
        "old_synthetic_token",
        "account_A",
        "http://127.0.0.1:1",
        1,
        old_expiry as u64,
        false,
    );
    assert!(old.has_account_fenced_credential());
    assert!(old_expiry > admission_clock + 30);
    let vk = VirtualKey {
        id: "key_owner".into(),
        upstream_ref: "backend/backend".into(),
        expires_at: Some(
            time::OffsetDateTime::from_unix_timestamp(clock + 400)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        ),
        ..Default::default()
    };
    let (store, slots) = crate::settlement::buffered::reference_authority(&state).unwrap();
    // Synthetic already-authorized source: dispatch was admitted31s ago, now it
    // finishes with3s left. No live renewal or historical clock manipulation.
    let issued = frozen_context(
        store,
        slots,
        &vk,
        &old,
        &request("model"),
        Family::OpenaiResponses,
        admission_clock,
    )
    .unwrap();
    assert_eq!(issued.expiry, clock + 400);
    assert!(issued.expiry > old_expiry);
    let refs=response_refs(&json!({"object":"response","id":"resp_issued","output":[{"type":"reasoning","id":"rs_issued","summary":[],"encrypted_content":"source_opaque"}]}),Family::OpenaiResponses).unwrap();
    issued.issue(refs).await.unwrap();
    let renewed = codex(
        "renewed_synthetic_token",
        "account_A",
        "http://127.0.0.1:1",
        2,
        (clock + 600) as u64,
        false,
    );
    assert!(renewed.credential_is_current());
    assert_eq!(
        old.routing_credential_fingerprint(),
        renewed.routing_credential_fingerprint()
    );
    while now() < old_expiry {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(now() >= old_expiry);
    assert!(!old.credential_is_current());
    assert!(prepare(
        &state,
        &vk,
        &old,
        &request("model"),
        IngressDialect::Responses
    )
    .await
    .is_err());
    let current = prepare(
        &state,
        &vk,
        &renewed,
        &request("model"),
        IngressDialect::Responses,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(issued.binding, current.binding);
    for changed in [
        codex(
            "renewed_synthetic_token",
            "account_B",
            "http://127.0.0.1:1",
            2,
            (clock + 600) as u64,
            false,
        ),
        codex(
            "renewed_synthetic_token",
            "account_A",
            "http://127.0.0.1:2",
            2,
            (clock + 600) as u64,
            false,
        ),
        codex(
            "renewed_synthetic_token",
            "account_A",
            "http://127.0.0.1:1",
            2,
            (clock + 600) as u64,
            true,
        ),
    ] {
        assert!(prepare(
            &state,
            &vk,
            &changed,
            &request("model"),
            IngressDialect::Responses
        )
        .await
        .is_err());
    }
    for field in ["backend", "revision", "credential", "profile"] {
        let mut binding = renewed.backend_binding().unwrap().clone();
        match field {
            "backend" => binding.id = "other".into(),
            "revision" => binding.revision += 1,
            "credential" => binding.credential_ref = "openai:other".into(),
            _ => binding.endpoint_profile = "standard".into(),
        };
        assert!(
            prepare(
                &state,
                &vk,
                &renewed.clone().with_backend_binding(binding),
                &request("model"),
                IngressDialect::Responses
            )
            .await
            .is_err(),
            "{field}"
        );
    }
    assert!(prepare(
        &state,
        &vk,
        &renewed,
        &request("other-model"),
        IngressDialect::Responses
    )
    .await
    .is_err());
    let expired = renewed.clone().with_credential_expiry((now() - 1) as u64);
    assert!(prepare(
        &state,
        &vk,
        &expired,
        &request("model"),
        IngressDialect::Responses
    )
    .await
    .is_err());
    // A header or expiry wrapper on an ordinary API-key transport is not a Codex fence.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("chatgpt-account-id", "account_A".parse().unwrap());
    let generic = ProviderRuntime::new()
        .openai_responses(
            "vendor",
            "http://127.0.0.1:1",
            "ordinary_synthetic_key",
            headers,
            Some(0),
            None,
            None,
            None,
        )
        .with_credential_expiry((now() + 60) as u64)
        .with_backend_binding(renewed.backend_binding().unwrap().clone());
    assert!(!generic.has_account_fenced_credential());
    let (store, slots) = crate::settlement::buffered::reference_authority(&state).unwrap();
    let ordinary = frozen_context(
        store,
        slots,
        &vk,
        &generic,
        &request("model"),
        Family::OpenaiResponses,
        now(),
    )
    .unwrap();
    assert_eq!(
        ordinary.expiry,
        generic.reference_credential_expiry().unwrap() as i64
    );
    assert!(prepare(
        &state,
        &vk,
        &generic,
        &request("model"),
        IngressDialect::Responses
    )
    .await
    .is_err());
    eprintln!("synthetic account renewal: actual old lease expiry passed; current renewed lease accepted; expired/unrenewed and changed binding refused");
}
