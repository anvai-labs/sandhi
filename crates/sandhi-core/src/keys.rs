//! Virtual keys — one shared upstream key fronts many per-user keys, so attribution and
//! revocation are **per person, not per shared secret** (AnvaiOps ADR-0047 D4).
//!
//! The store maps a virtual-key id (`vk_…`) to its subject/group attribution and an opaque
//! reference to the real upstream credential — **never the secret itself** (the proxy resolves
//! the reference to the held secret server-side via the vault — TD-0003).
//!
//! TD-0003 P1 adds optional scoping fields to [`VirtualKey`] (model allowlist, explicit budget
//! scope, expiry, rate limit) used by the operator share/mint path. They are all `Option` so the
//! legacy demo path (`insert` keyed by the plaintext id) is unchanged. Minted keys are stored
//! keyed by a **hash** of the presented secret (see [`KeyStore::insert_keyed`]) so the live table
//! never retains the plaintext secret as its lookup key.

use std::collections::HashMap;
use std::sync::Mutex;

/// A virtual key: what a caller presents instead of the real upstream key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VirtualKey {
    /// Public id the caller presents, e.g. `vk_abc123`. For operator-minted keys this is the
    /// stable public identifier (NOT the secret); for the legacy demo path it doubles as the
    /// lookup token.
    pub id: String,
    /// The user this key attributes usage to.
    pub subject_id: Option<String>,
    /// The team/group (also the default prompt-cache namespace — ADR-0047 D9).
    pub group_id: Option<String>,
    /// Opaque reference to the real upstream credential (a name/id, never the secret).
    pub upstream_ref: String,

    // --- TD-0003 P1 operator scoping (all optional; None = unscoped/legacy) ---
    /// Optional model allowlist. When set, only these models are admitted for this key.
    #[doc(hidden)]
    pub models: Option<Vec<String>>,
    /// Explicit budget scope (e.g. `user:alice`, `group:platform`). Overrides the default
    /// group/vk-derived scope when set.
    pub budget_scope: Option<String>,
    /// RFC 3339 expiry timestamp. A presented key past this instant is rejected.
    pub expires_at: Option<String>,
    /// Optional rate limit in requests/minute. Stored only in P1; enforcement is P2.
    pub rate_limit_per_min: Option<u32>,
}

impl VirtualKey {
    /// Whether this key has expired at `now_rfc3339` (lexicographic compare is correct for the
    /// fixed-width RFC 3339 `YYYY-MM-DDTHH:MM:SSZ` form). `None` expiry = never expires.
    pub fn is_expired(&self, now_rfc3339: &str) -> bool {
        self.expires_at
            .as_deref()
            .is_some_and(|exp| exp <= now_rfc3339)
    }

    /// Whether `model` is permitted by the optional model allowlist. No allowlist = any model.
    pub fn permits_model(&self, model: &str) -> bool {
        self.models
            .as_deref()
            .map(|allowed| allowed.iter().any(|m| m == model))
            .unwrap_or(true)
    }
}

/// An in-memory virtual-key store. Interior-mutable (the admin/mint path mutates it through a
/// shared `&Self`). The durable store lives in `sandhi-store`; this is the fast request-path
/// resolver.
#[derive(Debug, Default)]
pub struct KeyStore {
    map: Mutex<HashMap<String, VirtualKey>>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert keyed by the key's own `id` (the legacy/demo path: the presented token is the id).
    pub fn insert(&self, key: VirtualKey) {
        let mut map = self.map.lock().expect("key store poisoned");
        map.insert(key.id.clone(), key);
    }

    /// Insert keyed by an explicit lookup string. Used by the operator mint path, where the
    /// lookup key is a **hash** of the presented secret (never the plaintext) — so the live table
    /// never retains the secret as a key, mirroring the durable store.
    pub fn insert_keyed(&self, lookup_key: String, key: VirtualKey) {
        let mut map = self.map.lock().expect("key store poisoned");
        map.insert(lookup_key, key);
    }

    /// Resolve a presented token by exact lookup-key match. Returns a cloned [`VirtualKey`].
    pub fn resolve(&self, lookup_key: &str) -> Option<VirtualKey> {
        let map = self.map.lock().expect("key store poisoned");
        map.get(lookup_key).cloned()
    }

    /// Revoke by exact lookup key. Returns whether it existed.
    pub fn revoke(&self, lookup_key: &str) -> bool {
        let mut map = self.map.lock().expect("key store poisoned");
        map.remove(lookup_key).is_some()
    }

    pub fn len(&self) -> usize {
        self.map.lock().expect("key store poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
            ..Default::default()
        }
    }

    #[test]
    fn resolve_and_revoke() {
        let store = KeyStore::new();
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

    #[test]
    fn keyed_insert_uses_the_lookup_string_not_the_id() {
        // A minted key is looked up by a hash of the presented secret, independent of its id.
        let store = KeyStore::new();
        let key = VirtualKey {
            id: "key_abc".into(),
            upstream_ref: "openai:default".into(),
            ..Default::default()
        };
        store.insert_keyed("hash-of-secret".into(), key);
        // The plaintext id is NOT a lookup key…
        assert!(store.resolve("key_abc").is_none());
        // …the hash is.
        assert_eq!(store.resolve("hash-of-secret").unwrap().id, "key_abc");
    }

    #[test]
    fn expiry_and_model_allowlist_helpers() {
        let mut key = vk();
        assert!(!key.is_expired("2030-01-01T00:00:00Z"));
        key.expires_at = Some("2020-01-01T00:00:00Z".into());
        assert!(key.is_expired("2030-01-01T00:00:00Z"));

        let mut key = vk();
        assert!(key.permits_model("anything")); // no allowlist
        key.models = Some(vec!["claude-x".into(), "claude-y".into()]);
        assert!(key.permits_model("claude-x"));
        assert!(!key.permits_model("gpt-x"));
    }
}
