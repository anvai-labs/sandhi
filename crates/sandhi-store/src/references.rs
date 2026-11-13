//! Digest-only issuer proofs. No prompt, native payload, provider ID or credential is stored.
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{sync::Mutex, time::Duration};

pub const ENCRYPTED_REASONING: u8 = 3;
pub const THINKING_SIGNATURE: u8 = 6;
pub const REDACTED_THINKING: u8 = 7;
fn self_contained(kind: u8) -> bool {
    matches!(
        kind,
        ENCRYPTED_REASONING | THINKING_SIGNATURE | REDACTED_THINKING
    )
}

pub const MAX_BATCH: usize = 128;
pub const MAX_ROWS: i64 = 100_000;
pub const MAX_OWNER_ROWS: i64 = 4096;
pub const MAX_TTL_SECS: i64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    pub kind: u8,
    pub digest: String,
}
#[derive(Debug)]
pub struct Error;
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("native reference ownership is unavailable or unverified")
    }
}
impl std::error::Error for Error {}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self
    }
}

pub struct ReferenceStore {
    conn: Mutex<Connection>,
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn validate(binding: &str, owner: &str, refs: &[Reference]) -> Result<(), Error> {
    if !digest(binding)
        || !digest(owner)
        || refs.len() > MAX_BATCH
        || refs
            .iter()
            .any(|r| !(1..=16).contains(&r.kind) || !digest(&r.digest))
    {
        return Err(Error);
    }
    Ok(())
}
impl ReferenceStore {
    pub fn open(path: &str) -> Result<Self, Error> {
        if path.is_empty() || path == ":memory:" || path.starts_with("file:") {
            return Err(Error);
        }
        let conn = Connection::open(path)?;
        if conn.is_readonly("main")? {
            return Err(Error);
        }
        conn.busy_timeout(Duration::from_millis(100))?;
        // Issuance is authorization state: do not acknowledge it before durable commit.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS issued_native_references (
              binding TEXT NOT NULL CHECK(length(binding)=64),
              kind INTEGER NOT NULL CHECK(kind BETWEEN 1 AND 16),
              reference_digest TEXT NOT NULL CHECK(length(reference_digest)=64),
              owner TEXT NOT NULL CHECK(length(owner)=64),
              expires_at INTEGER NOT NULL,
              PRIMARY KEY(binding,kind,reference_digest,owner));
            CREATE UNIQUE INDEX IF NOT EXISTS native_resource_single_owner
            ON issued_native_references(binding,kind,reference_digest) WHERE kind NOT IN (3,6,7);
            CREATE INDEX IF NOT EXISTS native_references_expiry ON issued_native_references(expires_at);
            CREATE INDEX IF NOT EXISTS native_references_owner ON issued_native_references(owner);")?;
        let owner_pk: i64 = conn.query_row(
            "SELECT pk FROM pragma_table_info('issued_native_references') WHERE name='owner'",
            [],
            |r| r.get(0),
        )?;
        if owner_pk != 4 {
            return Err(Error);
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    /// All references must be positively issued to this exact scope. Expired rows grant nothing.
    pub fn authorize(&self, binding: &str, owner: &str, refs: &[Reference]) -> Result<(), Error> {
        self.authorize_at(binding, owner, refs, None)
    }
    fn authorize_at(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
    ) -> Result<(), Error> {
        validate(binding, owner, refs)?;
        let conn = self.conn.lock().map_err(|_| Error)?;
        for r in refs {
            let row: Option<(String,i64)> = conn.query_row(
                "SELECT owner,expires_at FROM issued_native_references WHERE binding=?1 AND kind=?2 AND reference_digest=?3 AND owner=?4",
                params![binding,r.kind,r.digest,owner], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            if !row.is_some_and(|(scope, expiry)| {
                scope == owner && expiry > clock.unwrap_or_else(unix_now)
            }) {
                return Err(Error);
            }
        }
        Ok(())
    }
    /// A provider acknowledgement may add proof, never transfer another live owner's proof.
    /// One complete frame/buffer commits atomically before its bytes are forwarded.
    pub fn issue(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        expires_at: i64,
    ) -> Result<(), Error> {
        self.issue_at(binding, owner, refs, None, expires_at)
    }
    fn issue_at(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
        expires_at: i64,
    ) -> Result<(), Error> {
        validate(binding, owner, refs)?;
        let mut conn = self.conn.lock().map_err(|_| Error)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = clock.unwrap_or_else(unix_now);
        if expires_at <= now || expires_at > now.saturating_add(MAX_TTL_SECS) {
            return Err(Error);
        }
        // Cleanup is bounded; capacity failure is fail closed, never an unbounded sweep.
        // Reserve half the cleanup budget for this owner so a global backlog
        // cannot leave its expired 4096-row quota permanently full on rollback.
        let mut cleaned=tx.execute("DELETE FROM issued_native_references WHERE rowid IN
          (SELECT rowid FROM issued_native_references WHERE owner=?1 AND expires_at<=?2 ORDER BY expires_at LIMIT 128)",params![owner,now])?;
        // Fresh source issuance can replace an expired resource-ID proof even
        // when its row is behind the bounded global oldest-first window.
        // Authorization never performs this deletion or adopts caller input.
        for reference in refs.iter().filter(|r| !self_contained(r.kind)) {
            cleaned+=tx.execute("DELETE FROM issued_native_references WHERE binding=?1 AND kind=?2 AND reference_digest=?3 AND expires_at<=?4",params![binding,reference.kind,reference.digest,now])?;
        }
        let remaining = 256usize.saturating_sub(cleaned);
        tx.execute("DELETE FROM issued_native_references WHERE rowid IN
          (SELECT rowid FROM issued_native_references WHERE expires_at<=?1 ORDER BY expires_at LIMIT ?2)",params![now,remaining as i64])?;
        let mut total: i64 =
            tx.query_row("SELECT COUNT(*) FROM issued_native_references", [], |r| {
                r.get(0)
            })?;
        let mut owned: i64 = tx.query_row(
            "SELECT COUNT(*) FROM issued_native_references WHERE owner=?1",
            [owner],
            |r| r.get(0),
        )?;
        for r in refs {
            // Self-contained payloads may be independently issued to multiple
            // owners. A resource ID has one owner; an ambiguous collision refuses.
            let previous: Option<(String, i64)> = if self_contained(r.kind) {
                tx.query_row("SELECT owner,expires_at FROM issued_native_references WHERE binding=?1 AND kind=?2 AND reference_digest=?3 AND owner=?4",params![binding,r.kind,r.digest,owner],|row|Ok((row.get(0)?,row.get(1)?))).optional()?
            } else {
                tx.query_row("SELECT owner,expires_at FROM issued_native_references WHERE binding=?1 AND kind=?2 AND reference_digest=?3",params![binding,r.kind,r.digest],|row|Ok((row.get(0)?,row.get(1)?))).optional()?
            };
            if previous
                .as_ref()
                .is_some_and(|(old_owner, _)| old_owner != owner)
            {
                return Err(Error);
            }
            if previous.is_none() {
                if total >= MAX_ROWS || owned >= MAX_OWNER_ROWS {
                    return Err(Error);
                }
                total += 1;
                owned += 1;
            }
            tx.execute(
                "INSERT INTO issued_native_references VALUES (?1,?2,?3,?4,?5)
                ON CONFLICT(binding,kind,reference_digest,owner) DO UPDATE SET
                expires_at=MAX(issued_native_references.expires_at,excluded.expires_at)",
                params![binding, r.kind, r.digest, owner, expires_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proof_is_scoped_atomic_expiring_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let path = path.to_str().unwrap();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let backend = "c".repeat(64);
        let reference = Reference {
            kind: 1,
            digest: "d".repeat(64),
        };
        let store = ReferenceStore::open(path).unwrap();
        assert!(store
            .authorize_at(&backend, &a, std::slice::from_ref(&reference), Some(10))
            .is_err());
        store
            .issue_at(&backend, &a, std::slice::from_ref(&reference), Some(10), 20)
            .unwrap();
        assert!(store
            .issue_at(&backend, &b, std::slice::from_ref(&reference), Some(11), 21)
            .is_err());
        drop(store);
        let store = ReferenceStore::open(path).unwrap();
        store
            .authorize_at(&backend, &a, std::slice::from_ref(&reference), Some(19))
            .unwrap();
        assert!(store
            .authorize_at(&backend, &b, std::slice::from_ref(&reference), Some(19))
            .is_err());
        assert!(store
            .authorize_at(&b, &a, std::slice::from_ref(&reference), Some(19))
            .is_err());
        assert!(store
            .authorize_at(&backend, &a, std::slice::from_ref(&reference), Some(20))
            .is_err());
        let next = Reference {
            kind: 2,
            digest: "e".repeat(64),
        };
        assert!(store
            .issue_at(
                &backend,
                &b,
                &[next.clone(), reference.clone()],
                Some(19),
                30
            )
            .is_err());
        assert!(store.authorize_at(&backend, &b, &[next], Some(19)).is_err());
        store
            .issue_at(&backend, &b, std::slice::from_ref(&reference), Some(21), 30)
            .unwrap();
        assert!(store
            .authorize_at(&backend, &a, &[reference], Some(21))
            .is_err());
    }
}

#[cfg(test)]
mod bounds_tests {
    use super::*;
    #[test]
    fn capacity_and_batch_refusal_never_overwrites_or_partly_adds_proof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let store = ReferenceStore::open(path.to_str().unwrap()).unwrap();
        let owner = "a".repeat(64);
        let binding = "b".repeat(64);
        let fresh = Reference {
            kind: 1,
            digest: "c".repeat(64),
        };
        assert!(store
            .issue_at(
                &binding,
                &owner,
                &vec![fresh.clone(); MAX_BATCH + 1],
                Some(10),
                20
            )
            .is_err());
        assert!(store
            .issue_at(
                &binding,
                &owner,
                std::slice::from_ref(&fresh),
                Some(10),
                10 + MAX_TTL_SECS + 1
            )
            .is_err());
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?1) INSERT INTO issued_native_references SELECT ?2,1,printf('%064x',x),?3,20 FROM n",params![MAX_OWNER_ROWS,binding,owner]).unwrap();
        }
        assert!(store
            .issue_at(&binding, &owner, std::slice::from_ref(&fresh), Some(10), 20)
            .is_err());
        let existing = Reference {
            kind: 1,
            digest: format!("{:064x}", 1),
        };
        store
            .issue_at(
                &binding,
                &owner,
                std::slice::from_ref(&existing),
                Some(10),
                20,
            )
            .unwrap();
        store
            .authorize_at(&binding, &owner, &[existing], Some(10))
            .unwrap();
        assert!(store
            .authorize_at(&binding, &owner, std::slice::from_ref(&fresh), Some(10))
            .is_err());
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("WITH RECURSIVE n(x) AS(SELECT ?1 UNION ALL SELECT x+1 FROM n WHERE x<?2) INSERT INTO issued_native_references SELECT ?3,1,printf('%064x',x),printf('%064x',x/4000),15 FROM n",params![MAX_OWNER_ROWS+1,MAX_ROWS,binding]).unwrap();
        }
        assert!(store
            .issue_at(
                &binding,
                &"d".repeat(64),
                std::slice::from_ref(&fresh),
                Some(10),
                20
            )
            .is_err());
        assert_eq!(
            store
                .conn
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM issued_native_references", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            MAX_ROWS
        );
        // All rows are expired, but other owners' rows expire earlier. Global
        // oldest-first cleanup alone cannot free this owner's full quota.
        store
            .issue_at(&binding, &owner, std::slice::from_ref(&fresh), Some(21), 30)
            .unwrap();
        store
            .authorize_at(&binding, &owner, &[fresh], Some(21))
            .unwrap();
        assert_eq!(
            store
                .conn
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM issued_native_references", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            MAX_ROWS - 255
        );
    }
}

#[cfg(test)]
mod reissuance_tests {
    use super::*;
    #[test]
    fn independent_source_issuance_adds_opaque_owner_proof_without_transferring_any_proof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let store = ReferenceStore::open(path.to_str().unwrap()).unwrap();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let binding = "c".repeat(64);
        for kind in [ENCRYPTED_REASONING, THINKING_SIGNATURE, REDACTED_THINKING] {
            let r = Reference {
                kind,
                digest: "d".repeat(64),
            };
            store
                .issue_at(&binding, &a, std::slice::from_ref(&r), Some(10), 20)
                .unwrap();
            assert!(store
                .authorize_at(&binding, &b, std::slice::from_ref(&r), Some(11))
                .is_err());
            store
                .issue_at(&binding, &b, std::slice::from_ref(&r), Some(11), 21)
                .unwrap();
            for owner in [&a, &b] {
                store
                    .authorize_at(&binding, owner, std::slice::from_ref(&r), Some(12))
                    .unwrap();
            }
        }
        let r = Reference {
            kind: 1,
            digest: "d".repeat(64),
        };
        store
            .issue_at(&binding, &a, std::slice::from_ref(&r), Some(10), 20)
            .unwrap();
        assert!(store
            .issue_at(&binding, &b, std::slice::from_ref(&r), Some(11), 21)
            .is_err());
        drop(store);
        let store = ReferenceStore::open(path.to_str().unwrap()).unwrap();
        for kind in [ENCRYPTED_REASONING, THINKING_SIGNATURE, REDACTED_THINKING] {
            let r = Reference {
                kind,
                digest: "d".repeat(64),
            };
            for owner in [&a, &b] {
                store
                    .authorize_at(&binding, owner, std::slice::from_ref(&r), Some(12))
                    .unwrap();
            }
        }
    }
}

#[cfg(test)]
mod targeted_expiry_tests {
    use super::*;
    #[test]
    fn fresh_source_reissuance_can_replace_a_late_expired_id_but_authorize_never_adopts_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let store = ReferenceStore::open(path.to_str().unwrap()).unwrap();
        let binding = "a".repeat(64);
        let old_owner = "b".repeat(64);
        let new_owner = "c".repeat(64);
        let now = unix_now();
        let conn = Connection::open(&path).unwrap();
        conn.execute("WITH RECURSIVE n(x) AS(SELECT 0 UNION ALL SELECT x+1 FROM n WHERE x<999) INSERT INTO issued_native_references SELECT ?1,1,printf('%064x',x),?2,?3-2000+x FROM n",params![binding,old_owner,now]).unwrap();
        let target = Reference {
            kind: 1,
            digest: format!("{:064x}", 999),
        };
        assert!(store
            .authorize(&binding, &new_owner, std::slice::from_ref(&target))
            .is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM issued_native_references", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            1000
        );
        store
            .issue(
                &binding,
                &new_owner,
                std::slice::from_ref(&target),
                now + 60,
            )
            .unwrap();
        store
            .authorize(&binding, &new_owner, std::slice::from_ref(&target))
            .unwrap();
        assert!(store
            .authorize(&binding, &old_owner, std::slice::from_ref(&target))
            .is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM issued_native_references", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            745
        );
        // A later failing live-ID collision must roll the targeted cleanup back too.
        let expired = Reference {
            kind: 1,
            digest: "d".repeat(64),
        };
        let live = Reference {
            kind: 1,
            digest: "e".repeat(64),
        };
        store
            .issue(&binding, &old_owner, std::slice::from_ref(&live), now + 60)
            .unwrap();
        conn.execute(
            "INSERT INTO issued_native_references VALUES(?1,1,?2,?3,?4)",
            params![binding, expired.digest, old_owner, now - 1],
        )
        .unwrap();
        assert!(store
            .issue(
                &binding,
                &new_owner,
                &[expired.clone(), live.clone()],
                now + 60
            )
            .is_err());
        assert_eq!(conn.query_row("SELECT count(*) FROM issued_native_references WHERE binding=?1 AND kind=1 AND reference_digest=?2 AND owner=?3",params![binding,expired.digest,old_owner],|row|row.get::<_,i64>(0)).unwrap(),1);
        assert!(store.authorize(&binding, &new_owner, &[expired]).is_err());
        store.authorize(&binding, &old_owner, &[live]).unwrap();
    }
    #[test]
    fn maximum_new_opaque_batch_progresses_when_owner_quota_and_older_global_backlog_expire() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refs.db");
        let store = ReferenceStore::open(path.to_str().unwrap()).unwrap();
        let binding = "a".repeat(64);
        let owner = "b".repeat(64);
        let other = "c".repeat(64);
        let now = unix_now();
        let conn = Connection::open(&path).unwrap();
        conn.execute("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<?1) INSERT INTO issued_native_references SELECT ?2,3,printf('%064x',x),?3,?4-1 FROM n",params![MAX_OWNER_ROWS,binding,owner,now]).unwrap();
        conn.execute("WITH RECURSIVE n(x) AS(SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) INSERT INTO issued_native_references SELECT ?1,3,printf('%064x',x),?2,?3-2 FROM n",params![binding,other,now]).unwrap();
        let refs = (MAX_OWNER_ROWS + 1..=MAX_OWNER_ROWS + MAX_BATCH as i64)
            .map(|n| Reference {
                kind: 3,
                digest: format!("{n:064x}"),
            })
            .collect::<Vec<_>>();
        store.issue(&binding, &owner, &refs, now + 60).unwrap();
        store.authorize(&binding, &owner, &refs).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM issued_native_references WHERE owner=?1",
                [owner],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            MAX_OWNER_ROWS
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM issued_native_references", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            MAX_OWNER_ROWS + 1000 - 128
        );
    }
}
