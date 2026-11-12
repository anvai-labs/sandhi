use sandhi_core::EndpointFamilyV1;
use sandhi_store::backends::{BackendDefinition, BackendEndpoint, BackendStore, BackendStoreError};
use serde_json::json;

#[test]
fn corrupt_backend_metadata_is_an_error_instead_of_empty_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backends.db");
    let store = BackendStore::open(path.to_str().unwrap()).unwrap();
    store.put(&definition(), None).unwrap();
    let database = rusqlite::Connection::open(path).unwrap();
    database
        .execute("UPDATE backends SET definition_json='{}'", [])
        .unwrap();
    assert!(matches!(store.list(), Err(BackendStoreError::Corrupt)));
    assert!(matches!(
        store.get("responses-primary"),
        Err(BackendStoreError::Corrupt)
    ));
}

fn definition() -> BackendDefinition {
    serde_json::from_value(json!({
        "id":"responses-primary","provider":"openai","credential_ref":"openai:production",
        "endpoint":{"endpoint_family":"openai_responses"},
        "models":[{"id":"fixture","capabilities":{"tools":true}}]
    }))
    .unwrap()
}

#[test]
fn cache_capabilities_are_strict_model_bound_and_revisioned() {
    let store = BackendStore::in_memory().unwrap();
    let mut wire = serde_json::to_value(definition()).unwrap();
    wire["cache_policy"] = json!({"default_intent":{"mode":"auto"},"namespace":"scoped_key",
        "models":{"fixture":{"profile":"openai_explicit","lifetimes":["30m","12h"],"max_breakpoints":4}}});
    let valid: BackendDefinition = serde_json::from_value(wire.clone()).unwrap();
    let first = store.put(&valid, None).unwrap();
    let mut changed = valid.clone();
    changed
        .cache_policy
        .as_mut()
        .unwrap()
        .allow_request_override = false;
    assert!(matches!(
        store.put(&changed, None),
        Err(BackendStoreError::Conflict)
    ));
    assert_eq!(
        store.put(&changed, Some(first.revision)).unwrap().revision,
        2
    );
    for (pointer, value) in [
        (
            "/cache_policy/models/fixture/profile",
            json!("anthropic_breakpoints"),
        ),
        ("/cache_policy/models/fixture/lifetimes", json!(["1h"])),
        ("/cache_policy/models/fixture/max_breakpoints", json!(5)),
        (
            "/cache_policy/models",
            json!({"unknown":{"profile":"native_only"}}),
        ),
    ] {
        let mut invalid = wire.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        let def: BackendDefinition = serde_json::from_value(invalid).unwrap();
        assert!(def.validate().is_err(), "{pointer}");
    }
    wire["endpoint"]["endpoint_profile"] = json!("chatgpt_codex");
    let invalid: BackendDefinition = serde_json::from_value(wire).unwrap();
    assert!(invalid.validate().is_err());
}

#[test]
fn backend_metadata_survives_restart_and_requires_revision_for_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("gateway.db");
    let store = BackendStore::open(path.to_str().unwrap()).unwrap();
    let def = definition();
    let first = store.put(&def, None).unwrap();
    assert_eq!(first.revision, 1);
    assert_eq!(
        store.put(&def, None).unwrap(),
        first,
        "exact create replay is idempotent"
    );
    drop(store);
    let store = BackendStore::open(path.to_str().unwrap()).unwrap();
    assert_eq!(store.get(&def.id).unwrap(), Some(first.clone()));
    assert_eq!(first.definition.route_ref(), "backend/responses-primary");
    let mut changed = def.clone();
    changed.endpoint = BackendEndpoint::standard(EndpointFamilyV1::OpenaiChatCompletions);
    assert!(matches!(
        store.put(&changed, None),
        Err(BackendStoreError::Conflict)
    ));
    assert!(matches!(
        store.put(&changed, Some(0)),
        Err(BackendStoreError::Conflict)
    ));
    let second = store.put(&changed, Some(1)).unwrap();
    assert_eq!(second.revision, 2);
    assert!(matches!(
        store.revoke(&def.id, 1),
        Err(BackendStoreError::Conflict)
    ));
    let revoked = store.revoke(&def.id, 2).unwrap();
    assert_eq!(revoked.revision, 3);
    assert_eq!(revoked.status, "revoked");
    assert!(matches!(
        store.put(&changed, None),
        Err(BackendStoreError::Conflict)
    ));
    let restored = store.put(&changed, Some(3)).unwrap();
    assert_eq!(restored.revision, 4);
    assert_eq!(restored.status, "active");
    assert_eq!(store.list().unwrap(), vec![restored]);
}

#[test]
fn backend_definitions_are_strict_nonsecret_and_bounded() {
    let def = definition();
    for (field, value) in [
        ("secret", json!("must-not-persist")),
        ("unknown", json!(true)),
    ] {
        let mut wire = serde_json::to_value(&def).unwrap();
        wire[field] = value;
        assert!(serde_json::from_value::<BackendDefinition>(wire).is_err());
    }
    let store = BackendStore::in_memory().unwrap();
    for (id, reference) in [
        ("../escape", "openai:production"),
        ("good", "openai"),
        ("good", "openai:production:extra"),
        ("good", "openai:../escape"),
    ] {
        let mut invalid = def.clone();
        invalid.id = id.into();
        invalid.credential_ref = reference.into();
        assert!(store.put(&invalid, None).is_err());
    }
    let mut duplicate = def.clone();
    duplicate.models.push(duplicate.models[0].clone());
    assert!(store.put(&duplicate, None).is_err());
    let mut unbounded = def.clone();
    unbounded.user_agent = Some("x".repeat(100_000));
    assert!(store.put(&unbounded, None).is_err());
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn concurrent_writers_cannot_overwrite_a_newer_backend_revision() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backends.db");
    let store = BackendStore::open(path.to_str().unwrap()).unwrap();
    let mut def = definition();
    store.put(&def, None).unwrap();
    let other = BackendStore::open(path.to_str().unwrap()).unwrap();
    def.user_agent = Some("first-update".into());
    store.put(&def, Some(1)).unwrap();
    def.user_agent = Some("stale-update".into());
    assert!(matches!(
        other.put(&def, Some(1)),
        Err(BackendStoreError::Conflict)
    ));
    assert_eq!(
        store
            .get(&def.id)
            .unwrap()
            .unwrap()
            .definition
            .user_agent
            .as_deref(),
        Some("first-update")
    );
}
