//! Durable gateway-profile store (per-group defaults + profile-download tokens).
//!
//! Two tables, one store, opened against the same SQLite file as the vault/vkeys/alerts stores:
//!
//! - `profiles` — the centrally-declared, per-group defaults applied from the declarative
//!   `profiles:` config section (`POST /admin/config/apply`). One row per group, additive-only
//!   upsert (never deletes a group).
//! - `profile_tokens` — bearer credentials for `GET /auth/profile`. Only a **hash** of the token
//!   is stored (the same unsalted SHA-256 hex scheme the vkeys table uses via
//!   [`crate::hash_secret`]), never the plaintext — so a database leak cannot download profiles
//!   or mint virtual keys. The plaintext `skp_…` token is shown exactly once at mint time.
//!
//! No dollars, no SKU/tier — only neutral defaults (seconds, tokens, model names).

use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::hash_secret;

/// A centrally-applied per-group profile (the assembled defaults, masked — carries no secrets:
/// no credential, no token, no virtual key. Masking is by construction; keep it that way).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileRecord {
    /// Owning attribution group (`RequestMetadataV1.group_id` vocabulary).
    pub group: String,
    /// `gateway` (callers go through this gateway with a minted virtual key) or `local`
    /// (callers run the models themselves; defaults + models only).
    pub mode: String,
    /// Upstream credential id (`provider:label`) a minted virtual key binds to. `None` for
    /// `local` profiles (and gateway profiles whose upstream is not yet registered).
    pub upstream_ref: Option<String>,
    /// Model allowlist exposed to the group.
    pub models: Vec<String>,
    pub timeout_seconds: Option<u64>,
    pub max_tokens: Option<u32>,
    pub updated_at: String,
}

/// A SQLite-backed profile + profile-token store.
pub struct ProfileStore {
    conn: Mutex<Connection>,
}

impl ProfileStore {
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
        // `group` is a SQL keyword — every statement quotes the column name.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS profiles (
                \"group\"         TEXT PRIMARY KEY,
                mode              TEXT NOT NULL,
                upstream_ref      TEXT,
                models_json       TEXT NOT NULL DEFAULT '[]',
                timeout_seconds   INTEGER,
                max_tokens        INTEGER,
                updated_at        TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS profile_tokens (
                token_hash        TEXT PRIMARY KEY,
                subject           TEXT NOT NULL,
                group_id          TEXT NOT NULL,
                created_at        TEXT NOT NULL,
                revoked_at        TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_profile_tokens_subject_group
                ON profile_tokens(subject, group_id);",
        )
    }

    /// Insert or update the profile for a group (additive-only upsert: this never deletes a
    /// group or downgrades a row the config no longer declares — callers simply stop calling it).
    pub fn upsert_profile(&self, record: &ProfileRecord) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("profile conn poisoned");
        conn.execute(
            "INSERT INTO profiles \
             (\"group\", mode, upstream_ref, models_json, timeout_seconds, max_tokens, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(\"group\") DO UPDATE SET \
             mode=excluded.mode, upstream_ref=excluded.upstream_ref, \
             models_json=excluded.models_json, timeout_seconds=excluded.timeout_seconds, \
             max_tokens=excluded.max_tokens, updated_at=excluded.updated_at",
            params![
                record.group,
                record.mode,
                record.upstream_ref,
                serde_json::to_string(&record.models).expect("Vec<String> always serializes"),
                record.timeout_seconds.map(|v| v as i64),
                record.max_tokens,
                record.updated_at,
            ],
        )?;
        Ok(())
    }

    /// The applied profile for one group, if any.
    pub fn profile_for_group(&self, group: &str) -> rusqlite::Result<Option<ProfileRecord>> {
        let conn = self.conn.lock().expect("profile conn poisoned");
        conn.query_row(
            "SELECT \"group\", mode, upstream_ref, models_json, timeout_seconds, max_tokens, \
             updated_at FROM profiles WHERE \"group\" = ?1",
            params![group],
            record_from_row,
        )
        .optional()
    }

    /// All applied profiles, ordered by group.
    pub fn list_profiles(&self) -> rusqlite::Result<Vec<ProfileRecord>> {
        let conn = self.conn.lock().expect("profile conn poisoned");
        let mut stmt = conn.prepare(
            "SELECT \"group\", mode, upstream_ref, models_json, timeout_seconds, max_tokens, \
             updated_at FROM profiles ORDER BY \"group\"",
        )?;
        let rows = stmt.query_map([], record_from_row)?;
        rows.collect()
    }

    /// Mint a profile-download token for `(subject, group)`. The plaintext `skp_…` token is
    /// returned **once** to the caller; only its SHA-256 hash is persisted.
    pub fn mint_profile_token(&self, subject: &str, group: &str) -> rusqlite::Result<String> {
        let token = generate_profile_token();
        let hash = hash_secret(&token);
        let conn = self.conn.lock().expect("profile conn poisoned");
        conn.execute(
            "INSERT INTO profile_tokens (token_hash, subject, group_id, created_at, revoked_at) \
             VALUES (?1, ?2, ?3, ?4, NULL)",
            params![hash, subject, group, now_rfc3339()],
        )?;
        Ok(token)
    }

    /// Resolve a presented token by hashing it. Active tokens only — a revoked token never
    /// resolves. Returns `(subject, group)`.
    pub fn lookup_active_profile_token(
        &self,
        presented: &str,
    ) -> rusqlite::Result<Option<(String, String)>> {
        let hash = hash_secret(presented);
        let conn = self.conn.lock().expect("profile conn poisoned");
        conn.query_row(
            "SELECT subject, group_id FROM profile_tokens \
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
    }

    /// Revoke every active profile token for `(subject, group)` — e.g. offboarding a user or
    /// rotating a leak. Returns how many active tokens were revoked.
    pub fn revoke_profile_tokens(&self, subject: &str, group: &str) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().expect("profile conn poisoned");
        let changed = conn.execute(
            "UPDATE profile_tokens SET revoked_at = ?1 \
             WHERE subject = ?2 AND group_id = ?3 AND revoked_at IS NULL",
            params![now_rfc3339(), subject, group],
        )?;
        Ok(changed)
    }
}

fn record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProfileRecord> {
    let models_json: String = r.get(3)?;
    Ok(ProfileRecord {
        group: r.get(0)?,
        mode: r.get(1)?,
        upstream_ref: r.get(2)?,
        models: serde_json::from_str(&models_json).unwrap_or_default(),
        timeout_seconds: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
        max_tokens: r.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        updated_at: r.get(6)?,
    })
}

/// `skp_` + 32 bytes of OS CSPRNG entropy, base64url — 256 bits, unguessable bearer credential.
/// A profile token is a LOOKUP INDEX (see `hash_secret`), so its security rests on entropy alone.
fn generate_profile_token() -> String {
    let mut buf = [0u8; 32];
    // getrandom reads from the OS CSPRNG; it is effectively infallible on the platforms Sandhi
    // targets. If it ever fails, panicking is correct — a token we cannot randomize is unsafe
    // to issue.
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    format!("skp_{}", URL_SAFE_NO_PAD.encode(buf))
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

    fn record(group: &str) -> ProfileRecord {
        ProfileRecord {
            group: group.into(),
            mode: "gateway".into(),
            upstream_ref: Some("ollama:default".into()),
            models: vec!["glm".into(), "claude-x".into()],
            timeout_seconds: Some(120),
            max_tokens: Some(8192),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn profile_upsert_lookup_round_trip() {
        let store = ProfileStore::in_memory().unwrap();
        assert!(store.profile_for_group("platform").unwrap().is_none());

        store.upsert_profile(&record("platform")).unwrap();
        let found = store.profile_for_group("platform").unwrap().unwrap();
        assert_eq!(found, record("platform"));

        // Upsert replaces the row (additive apply semantics: same group, updated fields).
        let mut updated = record("platform");
        updated.mode = "local".into();
        updated.upstream_ref = None;
        updated.max_tokens = None;
        updated.updated_at = "2026-02-01T00:00:00Z".into();
        store.upsert_profile(&updated).unwrap();
        assert_eq!(
            store.profile_for_group("platform").unwrap().unwrap(),
            updated
        );
        assert_eq!(store.list_profiles().unwrap().len(), 1);

        store.upsert_profile(&record("research")).unwrap();
        assert_eq!(store.list_profiles().unwrap().len(), 2);
        // Groups are independent — the platform row survived the research upsert.
        assert_eq!(
            store.profile_for_group("platform").unwrap().unwrap().mode,
            "local"
        );
    }

    #[test]
    fn profile_with_no_defaults_round_trips() {
        let store = ProfileStore::in_memory().unwrap();
        let minimal = ProfileRecord {
            group: "g".into(),
            mode: "local".into(),
            upstream_ref: None,
            models: Vec::new(),
            timeout_seconds: None,
            max_tokens: None,
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        store.upsert_profile(&minimal).unwrap();
        assert_eq!(store.profile_for_group("g").unwrap().unwrap(), minimal);
        assert_eq!(
            serde_json::to_value(store.profile_for_group("g").unwrap().unwrap()).unwrap()["models"],
            serde_json::json!([])
        );
    }

    #[test]
    fn profile_token_mint_lookup_revoke_lifecycle() {
        let store = ProfileStore::in_memory().unwrap();
        let token = store.mint_profile_token("alice", "platform").unwrap();

        // Shape: `skp_` prefix, base64url of 32 bytes = 256 bits of entropy.
        assert!(token.starts_with("skp_"), "token is skp_-prefixed: {token}");
        assert_eq!(
            token["skp_".len()..].len(),
            43,
            "32 bytes base64url: {token}"
        );
        assert!(!token.contains('+') && !token.contains('/') && !token.contains('='));

        // Active lookup resolves to the right identity.
        let (subject, group) = store
            .lookup_active_profile_token(&token)
            .unwrap()
            .expect("minted token resolves");
        assert_eq!((subject.as_str(), group.as_str()), ("alice", "platform"));

        // A wrong token never resolves.
        assert!(store
            .lookup_active_profile_token("skp_nope")
            .unwrap()
            .is_none());

        // Revoke — then the same token must stop resolving.
        assert_eq!(store.revoke_profile_tokens("alice", "platform").unwrap(), 1);
        assert!(store.lookup_active_profile_token(&token).unwrap().is_none());
        // Re-revoke counts nothing (already revoked).
        assert_eq!(store.revoke_profile_tokens("alice", "platform").unwrap(), 0);
        // A different (subject, group) was never touched.
        store.mint_profile_token("bob", "platform").unwrap();
        assert_eq!(store.revoke_profile_tokens("bob", "research").unwrap(), 0);
        assert_eq!(store.revoke_profile_tokens("bob", "platform").unwrap(), 1);
    }

    #[test]
    fn stored_token_hash_is_never_the_plaintext() {
        let store = ProfileStore::in_memory().unwrap();
        let token = store.mint_profile_token("alice", "platform").unwrap();
        let conn = store.conn.lock().unwrap();
        let stored: String = conn
            .query_row(
                "SELECT token_hash FROM profile_tokens WHERE subject = 'alice'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert_ne!(stored, token, "only the hash is persisted, never the token");
        assert_eq!(
            stored,
            crate::hash_secret(&token),
            "hash is the sha-256 hex scheme"
        );
        assert!(!stored.contains("skp_"));
    }

    #[test]
    fn minted_tokens_do_not_repeat() {
        let store = ProfileStore::in_memory().unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            let token = store.mint_profile_token("alice", "platform").unwrap();
            assert!(seen.insert(token), "tokens must not repeat (CSPRNG)");
        }
    }
}
