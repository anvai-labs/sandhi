//! Virtual keys — one shared upstream key fronts many per-user keys, so attribution and
//! revocation are **per person, not per shared secret** (AnvaiOps ADR-0047 D4).
//!
//! The store maps a virtual-key id (`vk_…`) to its subject/group attribution and an opaque
//! reference to the real upstream credential — **never the secret itself** (the proxy resolves
//! the reference to the held secret server-side).

use std::collections::HashMap;

/// A virtual key: what a caller presents instead of the real upstream key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualKey {
    /// Public id the caller presents, e.g. `vk_abc123`.
    pub id: String,
    /// The user this key attributes usage to.
    pub subject_id: Option<String>,
    /// The team/group (also the default prompt-cache namespace — ADR-0047 D9).
    pub group_id: Option<String>,
    /// Opaque reference to the real upstream credential (a name/id, never the secret).
    pub upstream_ref: String,
}

/// An in-memory virtual-key store (the durable store is a later milestone).
#[derive(Debug, Default)]
pub struct KeyStore {
    map: HashMap<String, VirtualKey>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: VirtualKey) {
        self.map.insert(key.id.clone(), key);
    }

    /// Resolve a presented `vk_…` to its attribution + upstream reference.
    pub fn resolve(&self, id: &str) -> Option<&VirtualKey> {
        self.map.get(id)
    }

    /// Revoke a key. Returns whether it existed.
    pub fn revoke(&mut self, id: &str) -> bool {
        self.map.remove(id).is_some()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vk() -> VirtualKey {
        VirtualKey {
            id: "vk_1".into(),
            subject_id: Some("alice".into()),
            group_id: Some("platform".into()),
            upstream_ref: "anthropic:default".into(),
        }
    }

    #[test]
    fn resolve_and_revoke() {
        let mut store = KeyStore::new();
        assert!(store.resolve("vk_1").is_none());
        store.insert(vk());
        assert_eq!(
            store.resolve("vk_1").unwrap().subject_id.as_deref(),
            Some("alice")
        );
        assert!(store.revoke("vk_1"));
        assert!(!store.revoke("vk_1"));
        assert!(store.is_empty());
    }
}
