//! Mandatory admission receipts. Contains metadata only, never prompts or matched values.
use rusqlite::{params, Connection, TransactionBehavior};
use sandhi_core::policy::{Decision, Identity};
use std::{path::Path, sync::Mutex, time::Duration};

pub struct PolicyAuditStore {
    connection: Mutex<Connection>,
    max_rows: u64,
}
impl PolicyAuditStore {
    pub fn open(path: &Path, max_rows: u64) -> rusqlite::Result<Self> {
        if max_rows == 0 || max_rows > 100_000 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_millis(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS policy_receipts (
              id TEXT PRIMARY KEY, created_at TEXT NOT NULL, issuer TEXT, subject_id TEXT,
              virtual_key_id TEXT NOT NULL, decision_json TEXT NOT NULL);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            max_rows,
        })
    }
    pub fn record(
        &self,
        identity: &Identity,
        key_id: &str,
        decision: &Decision,
    ) -> rusqlite::Result<String> {
        let json = serde_json::to_string(decision).map_err(|_| rusqlite::Error::InvalidQuery)?;
        if json.len() > 8192
            || key_id.len() > 256
            || identity.issuer.as_ref().is_some_and(|s| s.len() > 2048)
            || identity.subject.as_ref().is_some_and(|s| s.len() > 256)
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        // No unbounded mutex queue. Congestion is an unavailable mandatory sink.
        let mut connection = self
            .connection
            .try_lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: u64 = tx.query_row("SELECT count(*) FROM policy_receipts", [], |r| r.get(0))?;
        if count >= self.max_rows {
            return Err(rusqlite::Error::InvalidQuery);
        }
        let id:String = tx.query_row("INSERT INTO policy_receipts
            VALUES(lower(hex(randomblob(16))),strftime('%Y-%m-%dT%H:%M:%fZ','now'),?1,?2,?3,?4) RETURNING id",
            params![identity.issuer,identity.subject,key_id,json],|r|r.get(0))?;
        tx.commit()?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::policy::Disposition;
    #[test]
    fn receipts_survive_restart_and_capacity_is_mandatory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("audit.db");
        let decision = Decision {
            revision: 1,
            disposition: Disposition::Quarantine,
            matched_rules: vec!["test".into()],
            reason: "evaluated".into(),
            elapsed_us: 1,
        };
        let identity = Identity {
            subject: Some("alice".into()),
            ..Default::default()
        };
        let store = PolicyAuditStore::open(&path, 1).unwrap();
        assert_eq!(
            store
                .record(&identity, "public-id", &decision)
                .unwrap()
                .len(),
            32
        );
        drop(store);
        let store = PolicyAuditStore::open(&path, 1).unwrap();
        assert!(store.record(&identity, "public-id", &decision).is_err());
        let db = Connection::open(path).unwrap();
        assert_eq!(
            db.query_row("SELECT subject_id FROM policy_receipts", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "alice"
        );
    }
}
