//! Durable virtual-key store (TD-0003 P1, component 2).
//!
//! Operator-minted virtual keys persisted to SQLite. Only a **hash** of the presented secret is
//! stored (lookup), never the plaintext — so a database leak cannot recover virtual keys. The
//! live in-process resolver is `sandhi_core::KeyStore`; this durable store backs the admin API
//! (list / revoke across restarts) and rehydrates the live store on startup.
//!
//! No dollars, no SKU/tier — only neutral-token attribution wiring.

use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::hash_secret;

/// A persisted virtual-key record (masked — the secret is never present, only its hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VirtualKeyRecord {
    /// Stable public id (`key_<hex>`), used for attribution + listing/revoke.
    pub id: String,
    pub subject_id: Option<String>,
    pub group_id: Option<String>,
    /// Upstream credential id (`provider:label`) this key binds to.
    pub upstream_ref: String,
    pub models: Option<String>,
    pub budget_scope: Option<String>,
    pub expires_at: Option<String>,
    pub rate_limit_per_min: Option<u32>,
    /// SHA-256 hex of the presented secret.
    pub secret_hash: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

impl VirtualKeyRecord {
    /// Comma-joined model allowlist from the stored text column.
    pub fn model_list(&self) -> Vec<String> {
        self.models
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A minted key: the secret is returned **once** to the caller; only the hash is persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedKey {
    pub record: VirtualKeyRecord,
    /// The plaintext `vk_…` secret. Print/store once; Sandhi never persists it.
    pub secret: String,
}

/// Inputs to [`VirtualKeyStore::mint`].
#[derive(Debug, Clone)]
pub struct MintRequest {
    pub subject_id: Option<String>,
    pub group_id: Option<String>,
    pub upstream_ref: String,
    pub models: Vec<String>,
    pub budget_scope: Option<String>,
    pub expires_at: Option<String>,
    pub rate_limit_per_min: Option<u32>,
}

/// A SQLite-backed virtual-key store.
pub struct VirtualKeyStore {
    conn: Mutex<Connection>,
}

impl VirtualKeyStore {
    /// Open (creating if needed) a store at `path`.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Ephemeral in-memory store (tests / demos).
    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS virtual_keys (
                id                 TEXT PRIMARY KEY,
                subject_id         TEXT,
                group_id           TEXT,
                upstream_ref       TEXT NOT NULL,
                models             TEXT,
                budget_scope       TEXT,
                expires_at         TEXT,
                rate_limit_per_min INTEGER,
                secret_hash        TEXT NOT NULL UNIQUE,
                created_at         TEXT NOT NULL,
                revoked_at         TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_vkeys_hash ON virtual_keys(secret_hash);",
        )
    }

    /// Mint a new virtual key. The plaintext secret is generated, returned once, and only its
    /// SHA-256 hash is persisted.
    pub fn mint(&self, req: MintRequest) -> rusqlite::Result<MintedKey> {
        let secret = generate_secret();
        let hash = hash_secret(&secret);
        let id = generate_id();
        let models_csv = if req.models.is_empty() {
            None
        } else {
            Some(req.models.join(","))
        };
        let created_at = now_rfc3339();
        let record = VirtualKeyRecord {
            id: id.clone(),
            subject_id: req.subject_id,
            group_id: req.group_id,
            upstream_ref: req.upstream_ref,
            models: models_csv,
            budget_scope: req.budget_scope,
            expires_at: req.expires_at,
            rate_limit_per_min: req.rate_limit_per_min,
            secret_hash: hash.clone(),
            created_at,
            revoked_at: None,
        };
        let conn = self.conn.lock().expect("vkey conn poisoned");
        conn.execute(
            "INSERT INTO virtual_keys \
             (id, subject_id, group_id, upstream_ref, models, budget_scope, expires_at, \
              rate_limit_per_min, secret_hash, created_at, revoked_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,NULL)",
            params![
                record.id,
                record.subject_id,
                record.group_id,
                record.upstream_ref,
                record.models,
                record.budget_scope,
                record.expires_at,
                record.rate_limit_per_min,
                record.secret_hash,
                record.created_at,
            ],
        )?;
        Ok(MintedKey { record, secret })
    }

    /// Look up a record by hashing the presented secret. Excludes revoked keys.
    pub fn find_by_secret(
        &self,
        presented_secret: &str,
    ) -> rusqlite::Result<Option<VirtualKeyRecord>> {
        let hash = hash_secret(presented_secret);
        self.find_by_hash(&hash)
    }

    /// Look up a record by its stored secret hash. Excludes revoked keys.
    pub fn find_by_hash(&self, hash: &str) -> rusqlite::Result<Option<VirtualKeyRecord>> {
        let conn = self.conn.lock().expect("vkey conn poisoned");
        conn.query_row(
            "SELECT id, subject_id, group_id, upstream_ref, models, budget_scope, expires_at, \
             rate_limit_per_min, secret_hash, created_at, revoked_at \
             FROM virtual_keys WHERE secret_hash = ?1 AND revoked_at IS NULL",
            params![hash],
            record_from_row,
        )
        .optional()
    }

    /// Find a record by its public id.
    pub fn find_by_id(&self, id: &str) -> rusqlite::Result<Option<VirtualKeyRecord>> {
        let conn = self.conn.lock().expect("vkey conn poisoned");
        conn.query_row(
            "SELECT id, subject_id, group_id, upstream_ref, models, budget_scope, expires_at, \
             rate_limit_per_min, secret_hash, created_at, revoked_at \
             FROM virtual_keys WHERE id = ?1",
            params![id],
            record_from_row,
        )
        .optional()
    }

    /// All records (masked), newest first.
    pub fn list(&self) -> rusqlite::Result<Vec<VirtualKeyRecord>> {
        let conn = self.conn.lock().expect("vkey conn poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, subject_id, group_id, upstream_ref, models, budget_scope, expires_at, \
             rate_limit_per_min, secret_hash, created_at, revoked_at \
             FROM virtual_keys ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], record_from_row)?;
        rows.collect()
    }

    /// Revoke (soft-delete) by public id. Returns whether it existed and was active.
    pub fn revoke(&self, id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().expect("vkey conn poisoned");
        let changed = conn.execute(
            "UPDATE virtual_keys SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            params![now_rfc3339(), id],
        )?;
        Ok(changed > 0)
    }
}

fn record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<VirtualKeyRecord> {
    Ok(VirtualKeyRecord {
        id: r.get(0)?,
        subject_id: r.get(1)?,
        group_id: r.get(2)?,
        upstream_ref: r.get(3)?,
        models: r.get(4)?,
        budget_scope: r.get(5)?,
        expires_at: r.get(6)?,
        rate_limit_per_min: r.get(7)?,
        secret_hash: r.get(8)?,
        created_at: r.get(9)?,
        revoked_at: r.get(10)?,
    })
}

/// 32 hex chars (128 bits) of OS CSPRNG entropy — unguessable `vk_…` secret.
fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // getrandom reads from the OS CSPRNG; it is effectively infallible on the platforms Sandhi
    // targets. If it ever fails, panicking is correct — a key we cannot randomize is unsafe to
    // issue.
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    let mut out = String::with_capacity(buf.len() * 2);
    for byte in &buf {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn generate_secret() -> String {
    // 128 bits of entropy (16 bytes → 32 hex chars).
    format!("vk_{}", random_hex(16))
}

fn generate_id() -> String {
    // 48 bits of entropy is ample for a non-secret public id.
    format!("key_{}", random_hex(6))
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(upstream: &str) -> MintRequest {
        MintRequest {
            subject_id: Some("alice".into()),
            group_id: Some("platform".into()),
            upstream_ref: upstream.into(),
            models: vec!["claude-x".into()],
            budget_scope: Some("group:platform".into()),
            expires_at: None,
            rate_limit_per_min: Some(60),
        }
    }

    #[test]
    fn mint_then_present_resolves_with_scope() {
        let store = VirtualKeyStore::in_memory().unwrap();
        let minted = store.mint(req("anthropic:default")).unwrap();
        assert!(minted.secret.starts_with("vk_"));
        assert!(minted.record.id.starts_with("key_"));

        // Present the secret → resolves to the record with scope.
        let resolved = store
            .find_by_secret(&minted.secret)
            .unwrap()
            .expect("minted key resolves");
        assert_eq!(resolved.id, minted.record.id);
        assert_eq!(resolved.subject_id.as_deref(), Some("alice"));
        assert_eq!(resolved.upstream_ref, "anthropic:default");
        assert_eq!(resolved.budget_scope.as_deref(), Some("group:platform"));
        assert_eq!(resolved.rate_limit_per_min, Some(60));
        assert_eq!(resolved.model_list(), vec!["claude-x".to_string()]);

        // The persisted record never carries the plaintext secret.
        let serialized = serde_json::to_string(&resolved).unwrap();
        assert!(
            !serialized.contains(&minted.secret),
            "plaintext secret must never be persisted/serialized"
        );
        assert!(serialized.contains(resolved.secret_hash.as_str()));
    }

    #[test]
    fn revoked_key_no_longer_resolves() {
        let store = VirtualKeyStore::in_memory().unwrap();
        let minted = store.mint(req("openai:default")).unwrap();
        assert!(store.revoke(&minted.record.id).unwrap());

        // Hash lookup excludes revoked keys.
        assert!(store.find_by_secret(&minted.secret).unwrap().is_none());
        // The record still exists (marked revoked) for audit via find_by_id.
        let rec = store.find_by_id(&minted.record.id).unwrap().unwrap();
        assert!(rec.revoked_at.is_some());
        // Re-revoke is a no-op.
        assert!(!store.revoke(&minted.record.id).unwrap());
    }

    #[test]
    fn wrong_secret_does_not_resolve() {
        let store = VirtualKeyStore::in_memory().unwrap();
        let _minted = store.mint(req("anthropic:default")).unwrap();
        assert!(store.find_by_secret("vk_nope").unwrap().is_none());
    }

    #[test]
    fn list_returns_all_records_masked() {
        let store = VirtualKeyStore::in_memory().unwrap();
        let a = store.mint(req("anthropic:default")).unwrap();
        let _b = store.mint(req("openai:default")).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        let serialized = serde_json::to_string(&list).unwrap();
        assert!(!serialized.contains(&a.secret));
    }
}
