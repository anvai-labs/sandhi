use sandhi_store::routes::{Error, Pin, RouteDefinition, RouteStore, Target};
use serde_json::json;

fn definition() -> RouteDefinition {
    serde_json::from_value(json!({"id":"coding","models":[{"id":"agent","targets":[{"backend":"primary","model":"physical"}]}]})).unwrap()
}
fn pin(target: &str) -> Pin {
    Pin {
        namespace: "a".repeat(64),
        target: Target {
            backend: target.into(),
            model: "physical".into(),
        },
        fingerprint: "b".repeat(64),
        expires_at: 1000,
    }
}

#[test]
fn route_intent_is_strict_bounded_and_revisioned() {
    for invalid in [
        json!({"id":"coding","models":[],"api_key":"secret"}),
        json!({"id":"coding","models":[{"id":"agent","targets":[{"backend":"primary","model":"physical","url":"https://other"}]}]}),
        json!({"id":"coding","models":[{"id":"agent","targets":[]}]}),
    ] {
        let parsed = serde_json::from_value::<RouteDefinition>(invalid);
        assert!(parsed.is_err() || parsed.unwrap().validate().is_err());
    }
    let store = RouteStore::in_memory().unwrap();
    let mut desired = definition();
    let first = store.put(&desired, None).unwrap();
    assert_eq!(store.put(&desired, None).unwrap(), first);
    desired.models[0].targets[0].model = "other".into();
    assert!(matches!(store.put(&desired, None), Err(Error::Conflict)));
    let second = store.put(&desired, Some(first.revision)).unwrap();
    assert_eq!(second.revision, 2);
    assert!(matches!(store.revoke("coding", 1), Err(Error::Conflict)));
    let revoked = store.revoke("coding", 2).unwrap();
    assert_eq!(revoked.revision, 3);
    assert!(matches!(
        store.confirm(&second, None, 100),
        Err(Error::Conflict)
    ));
    assert_eq!(store.put(&desired, Some(3)).unwrap().revision, 4);
}

#[test]
fn persisted_pin_survives_reopen_and_never_silently_changes_target() {
    let path = std::env::temp_dir().join(format!(
        "sandhi-route-store-{}-{}.db",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let path = path.to_str().unwrap();
    {
        let store = RouteStore::open(path).unwrap();
        let record = store.put(&definition(), None).unwrap();
        store.confirm(&record, Some(&pin("primary")), 100).unwrap();
    }
    let store = RouteStore::open(path).unwrap();
    let record = store.get("coding").unwrap().unwrap();
    assert_eq!(
        store.pin(&"a".repeat(64), "coding", 101).unwrap(),
        Some(pin("primary"))
    );
    assert!(matches!(
        store.confirm(&record, Some(&pin("other")), 101),
        Err(Error::Conflict | Error::Invalid(_))
    ));
    assert_eq!(store.pin(&"a".repeat(64), "coding", 1000).unwrap(), None);
    let mut replacement = pin("primary");
    replacement.fingerprint = "c".repeat(64);
    replacement.expires_at = 2000;
    store.confirm(&record, Some(&replacement), 1000).unwrap();
    assert_eq!(
        store.pin(&replacement.namespace, "coding", 1001).unwrap(),
        Some(replacement)
    );
    drop(store);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn concurrent_first_turns_choose_one_durable_affinity() {
    let store = std::sync::Arc::new(RouteStore::in_memory().unwrap());
    let mut desired = definition();
    desired.models[0].targets.push(Target {
        backend: "other".into(),
        model: "physical".into(),
    });
    let record = store.put(&desired, None).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let tasks = ["primary", "other"].map(|target| {
        let (store, barrier, record) = (store.clone(), barrier.clone(), record.clone());
        std::thread::spawn(move || {
            barrier.wait();
            store.confirm(&record, Some(&pin(target)), 100)
        })
    });
    let outcomes = tasks.map(|task| task.join().unwrap());
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, Err(Error::Conflict)))
            .count(),
        1
    );
    assert!(store.pin(&"a".repeat(64), "coding", 100).unwrap().is_some());
}
