//! Durable alert-rule store (TD-0003 P2, component 3).
//!
//! Persists [`AlertRule`]s + `last_fired_at` to SQLite so threshold rules survive restarts and the
//! `last_fired_at` dedup (see `sandhi_core::AlertRegistry`) persists across proxy restarts. The
//! live evaluation engine is the in-memory [`sandhi_core::AlertRegistry`]; this store backs the
//! admin API (create / list / ack) and rehydrates the registry on startup.
//!
//! No dollars, no SKU/tier — only neutral-token thresholds.

use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use sandhi_core::{AlertChannel, AlertRule};
use serde::Serialize;

/// A persisted alert rule (+ firing/ack state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlertRuleRecord {
    pub id: String,
    pub scope: String,
    pub threshold_pct: u8,
    /// Channel as the wire spelling (`log` / `webhook:<url>`).
    pub channel: String,
    pub created_at: String,
    pub last_fired_at: Option<String>,
    pub acked_at: Option<String>,
}

impl AlertRuleRecord {
    /// Decode the stored wire spelling back into the core enum.
    pub fn channel_enum(&self) -> AlertChannel {
        AlertChannel::parse(&self.channel)
    }

    /// The [`AlertRule`] view used by the live registry.
    pub fn to_rule(&self) -> AlertRule {
        AlertRule {
            id: self.id.clone(),
            scope: self.scope.clone(),
            threshold_pct: self.threshold_pct,
            channel: self.channel_enum(),
        }
    }
}

/// Inputs to [`AlertStore::create`].
#[derive(Debug, Clone)]
pub struct CreateAlertRequest {
    pub scope: String,
    pub threshold_pct: u8,
    pub channel: AlertChannel,
}

/// A SQLite-backed alert-rule store.
pub struct AlertStore {
    conn: Mutex<Connection>,
}

impl AlertStore {
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
            "CREATE TABLE IF NOT EXISTS alert_rules (
                id              TEXT PRIMARY KEY,
                scope           TEXT NOT NULL,
                threshold_pct   INTEGER NOT NULL,
                channel         TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                last_fired_at   TEXT,
                acked_at        TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_alert_rules_scope ON alert_rules(scope);",
        )
    }

    /// Create a rule. Returns the persisted record.
    pub fn create(&self, req: CreateAlertRequest) -> rusqlite::Result<AlertRuleRecord> {
        let id = generate_id();
        let created_at = now_rfc3339();
        let channel = req.channel.as_str();
        let conn = self.conn.lock().expect("alert conn poisoned");
        conn.execute(
            "INSERT INTO alert_rules (id, scope, threshold_pct, channel, created_at, last_fired_at, acked_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)",
            params![id, req.scope, req.threshold_pct, channel, created_at],
        )?;
        drop(conn);
        self.find_by_id(&id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)
    }

    /// All rules, newest first.
    pub fn list(&self) -> rusqlite::Result<Vec<AlertRuleRecord>> {
        let conn = self.conn.lock().expect("alert conn poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, scope, threshold_pct, channel, created_at, last_fired_at, acked_at \
             FROM alert_rules ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], record_from_row)?;
        rows.collect()
    }

    /// Rules scoped to a single budget scope (used to rehydrate the registry selectively).
    pub fn list_by_scope(&self, scope: &str) -> rusqlite::Result<Vec<AlertRuleRecord>> {
        let conn = self.conn.lock().expect("alert conn poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, scope, threshold_pct, channel, created_at, last_fired_at, acked_at \
             FROM alert_rules WHERE scope = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![scope], record_from_row)?;
        rows.collect()
    }

    /// Find a rule by id.
    pub fn find_by_id(&self, id: &str) -> rusqlite::Result<Option<AlertRuleRecord>> {
        let conn = self.conn.lock().expect("alert conn poisoned");
        conn.query_row(
            "SELECT id, scope, threshold_pct, channel, created_at, last_fired_at, acked_at \
             FROM alert_rules WHERE id = ?1",
            params![id],
            record_from_row,
        )
        .optional()
    }

    /// Record the most-recent fire timestamp for a rule (mirrors the in-memory registry dedup).
    pub fn mark_fired(&self, id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().expect("alert conn poisoned");
        let changed = conn.execute(
            "UPDATE alert_rules SET last_fired_at = ?1 WHERE id = ?2",
            params![now_rfc3339(), id],
        )?;
        Ok(changed > 0)
    }

    /// Acknowledge a fired alert (clears it from the operator's attention queue).
    pub fn ack(&self, id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().expect("alert conn poisoned");
        let changed = conn.execute(
            "UPDATE alert_rules SET acked_at = ?1 WHERE id = ?2 AND acked_at IS NULL",
            params![now_rfc3339(), id],
        )?;
        Ok(changed > 0)
    }

    /// Delete a rule.
    pub fn delete(&self, id: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().expect("alert conn poisoned");
        let changed = conn.execute("DELETE FROM alert_rules WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }
}

fn record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AlertRuleRecord> {
    Ok(AlertRuleRecord {
        id: r.get(0)?,
        scope: r.get(1)?,
        threshold_pct: r.get::<_, i64>(2)? as u8,
        channel: r.get(3)?,
        created_at: r.get(4)?,
        last_fired_at: r.get(5)?,
        acked_at: r.get(6)?,
    })
}

/// 48 bits of OS CSPRNG entropy (non-secret public id).
fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    let mut out = String::with_capacity(buf.len() * 2);
    for byte in &buf {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn generate_id() -> String {
    format!("alert_{}", random_hex(6))
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

    fn req(scope: &str, pct: u8) -> CreateAlertRequest {
        CreateAlertRequest {
            scope: scope.into(),
            threshold_pct: pct,
            channel: AlertChannel::Log,
        }
    }

    #[test]
    fn create_list_find_ack_round_trip() {
        let store = AlertStore::in_memory().unwrap();
        let rec = store.create(req("group:platform", 80)).unwrap();
        assert!(rec.id.starts_with("alert_"));
        assert_eq!(rec.scope, "group:platform");
        assert_eq!(rec.threshold_pct, 80);
        assert_eq!(rec.channel_enum(), AlertChannel::Log);
        assert!(rec.last_fired_at.is_none());
        assert!(rec.acked_at.is_none());

        // List reflects it.
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);

        // find_by_id round-trips.
        let found = store.find_by_id(&rec.id).unwrap().unwrap();
        assert_eq!(found.scope, "group:platform");

        // mark_fired then ack.
        assert!(store.mark_fired(&rec.id).unwrap());
        assert!(store
            .find_by_id(&rec.id)
            .unwrap()
            .unwrap()
            .last_fired_at
            .is_some());
        assert!(store.ack(&rec.id).unwrap());
        assert!(store
            .find_by_id(&rec.id)
            .unwrap()
            .unwrap()
            .acked_at
            .is_some());
        // ack is idempotent (second ack → false).
        assert!(!store.ack(&rec.id).unwrap());
    }

    #[test]
    fn list_by_scope_isolates() {
        let store = AlertStore::in_memory().unwrap();
        store.create(req("group:a", 80)).unwrap();
        store.create(req("group:b", 90)).unwrap();
        assert_eq!(store.list_by_scope("group:a").unwrap().len(), 1);
        assert_eq!(store.list_by_scope("group:b").unwrap().len(), 1);
        assert_eq!(store.list_by_scope("group:c").unwrap().len(), 0);
    }

    #[test]
    fn webhook_channel_round_trips_through_store() {
        let store = AlertStore::in_memory().unwrap();
        let rec = store
            .create(CreateAlertRequest {
                scope: "group:w".into(),
                threshold_pct: 50,
                channel: AlertChannel::Webhook {
                    url: "https://hooks.example/x".into(),
                },
            })
            .unwrap();
        let rule = rec.to_rule();
        match rule.channel {
            AlertChannel::Webhook { url } => assert_eq!(url, "https://hooks.example/x"),
            _ => panic!("expected webhook channel"),
        }
    }

    #[test]
    fn delete_removes_rule() {
        let store = AlertStore::in_memory().unwrap();
        let rec = store.create(req("group:d", 70)).unwrap();
        assert!(store.delete(&rec.id).unwrap());
        assert!(store.find_by_id(&rec.id).unwrap().is_none());
        assert!(!store.delete(&rec.id).unwrap());
    }
}
