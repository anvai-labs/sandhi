//! Durable SQLite store for Sandhi — the usage-event sink + aggregation queries, plus the
//! operator tables introduced by TD-0003 (the [`vault`] credential index, [`vkeys`]
//! virtual-key store, and [`alerts`] threshold rules). Kept in its own crate (not `sandhi-core`)
//! so the language bindings' wheels never pull in bundled SQLite.

pub mod alerts;
pub mod ledger;
pub mod vault;
pub mod vkeys;

use std::sync::Mutex;

use rusqlite::{params, Connection};
use sandhi_core::{Backend, Sink, UsageEvent};
use serde::Serialize;

pub use alerts::{AlertRuleRecord, AlertStore, CreateAlertRequest};
pub use ledger::{ReserveOutcome, SqliteLedger};
pub use vault::{
    hash_secret, CredentialScheme, InMemoryVault, KeyringVault, SentinelPassVault, Vault,
    VaultEntry, VaultError, VaultStore,
};
pub use vkeys::{MintRequest, MintedKey, VirtualKeyRecord, VirtualKeyStore};

/// One aggregation row (or the grand total).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bucket {
    /// The group key (a subject/group/provider, or `"total"` / `"(none)"`).
    pub key: String,
    pub calls: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
}

impl Bucket {
    /// Total billable tokens (fresh in + out) — the neutral quantity for ranking/display.
    pub fn billable_tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }
}

/// A SQLite-backed usage store.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating if needed) a store at `path`.
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// An ephemeral in-memory store (tests / demos).
    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_events (
                request_id TEXT, occurred_at TEXT, provider TEXT, model TEXT, backend TEXT,
                virtual_key_id TEXT, subject_id TEXT, group_id TEXT, route TEXT, session_id TEXT,
                tokens_in INTEGER, tokens_out INTEGER,
                cache_creation_tokens INTEGER, cache_read_tokens INTEGER, gpu_seconds REAL
            );
            CREATE INDEX IF NOT EXISTS idx_usage_subject ON usage_events(subject_id);
            CREATE INDEX IF NOT EXISTS idx_usage_group ON usage_events(group_id);
            CREATE INDEX IF NOT EXISTS idx_usage_provider ON usage_events(provider);
            CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_events(model);
            CREATE INDEX IF NOT EXISTS idx_usage_vkey ON usage_events(virtual_key_id);
            CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_events(session_id);
            CREATE INDEX IF NOT EXISTS idx_usage_occurred ON usage_events(occurred_at);",
        )
    }

    fn insert(&self, e: &UsageEvent) -> rusqlite::Result<()> {
        let backend = match e.backend {
            Backend::External => "external",
            Backend::SelfHosted => "self_hosted",
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO usage_events (
                request_id, occurred_at, provider, model, backend,
                virtual_key_id, subject_id, group_id, route, session_id,
                tokens_in, tokens_out, cache_creation_tokens, cache_read_tokens, gpu_seconds
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                e.request_id,
                e.occurred_at,
                e.provider,
                e.model,
                backend,
                e.virtual_key_id,
                e.subject_id,
                e.group_id,
                e.route,
                e.session_id,
                e.tokens_in as i64,
                e.tokens_out as i64,
                e.cache_creation_tokens as i64,
                e.cache_read_tokens as i64,
                e.gpu_seconds,
            ],
        )?;
        Ok(())
    }

    /// Totals grouped by a fixed column (`subject_id` / `group_id` / `provider` / `model` /
    /// `virtual_key_id` / `session_id`), busiest first. An optional RFC 3339 `since` lower-bounds
    /// `occurred_at`.
    fn totals_grouped_since(
        &self,
        col: &str,
        since: Option<&str>,
    ) -> rusqlite::Result<Vec<Bucket>> {
        let conn = self.conn.lock().unwrap();
        let (where_clause, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = match since {
            Some(s) => (
                "WHERE occurred_at >= ?1".into(),
                vec![Box::new(s.to_string())],
            ),
            None => ("WHERE 1=1".into(), vec![]),
        };
        let sql = format!(
            "SELECT COALESCE({col}, '(none)') AS k, COUNT(*), \
                COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), COALESCE(SUM(cache_read_tokens),0) \
             FROM usage_events {where_clause} GROUP BY k \
             ORDER BY (COALESCE(SUM(tokens_in),0)+COALESCE(SUM(tokens_out),0)) DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |r| {
            Ok(Bucket {
                key: r.get(0)?,
                calls: r.get::<_, i64>(1)? as u64,
                tokens_in: r.get::<_, i64>(2)? as u64,
                tokens_out: r.get::<_, i64>(3)? as u64,
                cache_read_tokens: r.get::<_, i64>(4)? as u64,
            })
        })?;
        rows.collect()
    }

    /// Totals grouped by a fixed column, busiest first (no time window).
    fn totals_grouped(&self, col: &str) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped_since(col, None)
    }

    pub fn totals_by_subject(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("subject_id")
    }

    pub fn totals_by_group(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("group_id")
    }

    pub fn totals_by_provider(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("provider")
    }

    /// TD-0003 P1 attribution: per-model totals.
    pub fn totals_by_model(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("model")
    }

    /// TD-0003 P1 attribution: per-virtual-key totals.
    pub fn totals_by_virtual_key(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("virtual_key_id")
    }

    /// TD-0003 P1 attribution: per-session totals.
    pub fn totals_by_session(&self) -> rusqlite::Result<Vec<Bucket>> {
        self.totals_grouped("session_id")
    }

    /// Windowed variant: totals since an RFC 3339 timestamp, grouped by `dimension`
    /// (`subject` | `group` | `provider` | `model` | `key` | `session`). Returns `None` for an
    /// unknown dimension (the caller maps the short name).
    pub fn totals_since(
        &self,
        dimension: &str,
        since: &str,
    ) -> rusqlite::Result<Option<Vec<Bucket>>> {
        let col = match dimension {
            "subject" | "user" => "subject_id",
            "group" => "group_id",
            "provider" => "provider",
            "model" => "model",
            "key" | "virtual_key" => "virtual_key_id",
            "session" => "session_id",
            _ => return Ok(None),
        };
        Ok(Some(self.totals_grouped_since(col, Some(since))?))
    }

    /// The grand total across every event.
    pub fn grand_total(&self) -> rusqlite::Result<Bucket> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(tokens_in),0), COALESCE(SUM(tokens_out),0), \
                COALESCE(SUM(cache_read_tokens),0) FROM usage_events",
            [],
            |r| {
                Ok(Bucket {
                    key: "total".to_string(),
                    calls: r.get::<_, i64>(0)? as u64,
                    tokens_in: r.get::<_, i64>(1)? as u64,
                    tokens_out: r.get::<_, i64>(2)? as u64,
                    cache_read_tokens: r.get::<_, i64>(3)? as u64,
                })
            },
        )
    }
}

impl Sink for SqliteStore {
    fn emit(&self, event: &UsageEvent) {
        // Best-effort — a storage failure must never break the caller (ADR-0047 D7).
        let _ = self.insert(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::UsageEvent;

    fn ev(provider: &str, subject: &str, group: &str, tin: u64, tout: u64) -> UsageEvent {
        UsageEvent::new(
            "r",
            "2026-07-19T00:00:00Z",
            provider,
            "m",
            Backend::External,
        )
        .with_attribution(Some("vk".into()), Some(subject.into()), Some(group.into()))
        .with_tokens(tin, tout)
        .with_cache(0, 5)
    }

    #[test]
    fn persists_and_aggregates() {
        let store = SqliteStore::in_memory().unwrap();
        store.emit(&ev("openai", "alice", "team-a", 100, 20));
        store.emit(&ev("openai", "alice", "team-a", 50, 10));
        store.emit(&ev("anthropic", "bob", "team-b", 200, 40));

        let total = store.grand_total().unwrap();
        assert_eq!(total.calls, 3);
        assert_eq!(total.tokens_in, 350);
        assert_eq!(total.tokens_out, 70);
        assert_eq!(total.cache_read_tokens, 15);

        let by_subject = store.totals_by_subject().unwrap();
        // bob (240 billable) ranks above alice (180)
        assert_eq!(by_subject[0].key, "bob");
        assert_eq!(by_subject[0].billable_tokens(), 240);
        let alice = by_subject.iter().find(|b| b.key == "alice").unwrap();
        assert_eq!(alice.calls, 2);
        assert_eq!(alice.tokens_in, 150);

        let by_provider = store.totals_by_provider().unwrap();
        assert_eq!(by_provider.len(), 2);
        let openai = by_provider.iter().find(|b| b.key == "openai").unwrap();
        assert_eq!(openai.calls, 2);
    }

    #[test]
    fn empty_store_is_zero() {
        let store = SqliteStore::in_memory().unwrap();
        let total = store.grand_total().unwrap();
        assert_eq!(total.calls, 0);
        assert_eq!(total.billable_tokens(), 0);
        assert!(store.totals_by_group().unwrap().is_empty());
    }
}
