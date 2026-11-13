//! Digest-only issuer proofs. No prompt, native payload, provider ID or credential is stored.
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{
    sync::{Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant},
};

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

#[derive(Debug)]
enum StoreFailure {
    Unverified,
    Busy,
}
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
impl From<rusqlite::Error> for StoreFailure {
    fn from(error: rusqlite::Error) -> Self {
        if matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ) {
            Self::Busy
        } else {
            Self::Unverified
        }
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
fn validate(binding: &str, owner: &str, refs: &[Reference]) -> Result<(), StoreFailure> {
    if !digest(binding)
        || !digest(owner)
        || refs.len() > MAX_BATCH
        || refs
            .iter()
            .any(|r| !(1..=16).contains(&r.kind) || !digest(&r.digest))
    {
        return Err(StoreFailure::Unverified);
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

    /// Retry only the identical metadata lookup on transient contention. The
    /// caller supplies its original deadline, including any later shutdown cut.
    pub fn authorize_until(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        deadline: impl Fn() -> Instant,
    ) -> Result<(), Error> {
        self.until(&deadline, || {
            self.authorize_inner(binding, owner, refs, None, Some(&deadline))
        })
        .map_err(|_| Error)
    }

    fn until(
        &self,
        deadline: &dyn Fn() -> Instant,
        mut attempt: impl FnMut() -> Result<(), StoreFailure>,
    ) -> Result<(), StoreFailure> {
        let mut contended = false;
        loop {
            if Instant::now() >= deadline() {
                return Err(StoreFailure::Busy);
            }
            match attempt() {
                Err(StoreFailure::Busy) => {
                    if !contended {
                        tracing::debug!(
                            reason = "busy_or_locked",
                            "retrying identical native reference metadata within original deadline"
                        );
                        contended = true;
                    }
                    std::thread::sleep(
                        Duration::from_millis(1)
                            .min(deadline().saturating_duration_since(Instant::now())),
                    )
                }
                result => return result,
            }
        }
    }

    fn connection(
        &self,
        deadline: Option<&dyn Fn() -> Instant>,
    ) -> Result<MutexGuard<'_, Connection>, StoreFailure> {
        let conn = if let Some(deadline) = deadline {
            loop {
                if Instant::now() >= deadline() {
                    return Err(StoreFailure::Busy);
                }
                match self.conn.try_lock() {
                    Ok(conn) => break conn,
                    Err(TryLockError::Poisoned(_)) => return Err(StoreFailure::Unverified),
                    Err(TryLockError::WouldBlock) => std::thread::sleep(
                        Duration::from_millis(1)
                            .min(deadline().saturating_duration_since(Instant::now())),
                    ),
                }
            }
        } else {
            self.conn.lock().map_err(|_| StoreFailure::Unverified)?
        };
        // A busy handler cannot observe a newly established shutdown deadline.
        // Deadline-aware operations instead retry outside SQLite's writer wait.
        conn.busy_timeout(if deadline.is_some() {
            Duration::ZERO
        } else {
            Duration::from_millis(100)
        })?;
        Ok(conn)
    }

    fn authorize_at(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
    ) -> Result<(), Error> {
        self.authorize_inner(binding, owner, refs, clock, None)
            .map_err(|_| Error)
    }
    fn authorize_inner(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
        deadline: Option<&dyn Fn() -> Instant>,
    ) -> Result<(), StoreFailure> {
        validate(binding, owner, refs)?;
        let conn = self.connection(deadline)?;
        for r in refs {
            let row: Option<(String,i64)> = conn.query_row(
                "SELECT owner,expires_at FROM issued_native_references WHERE binding=?1 AND kind=?2 AND reference_digest=?3 AND owner=?4",
                params![binding,r.kind,r.digest,owner], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            if !row.is_some_and(|(scope, expiry)| {
                scope == owner && expiry > clock.unwrap_or_else(unix_now)
            }) {
                return Err(StoreFailure::Unverified);
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

    /// Contention retries never change the owner, reference batch or expiry and
    /// never expose bytes before the original atomic proof transaction commits.
    pub fn issue_until(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        expires_at: i64,
        deadline: impl Fn() -> Instant,
    ) -> Result<(), Error> {
        self.until(&deadline, || {
            self.issue_inner(binding, owner, refs, None, expires_at, Some(&deadline))
        })
        .map_err(|_| Error)
    }
    fn issue_at(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
        expires_at: i64,
    ) -> Result<(), Error> {
        self.issue_inner(binding, owner, refs, clock, expires_at, None)
            .map_err(|_| Error)
    }
    #[allow(clippy::too_many_arguments)]
    fn issue_inner(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
        expires_at: i64,
        deadline: Option<&dyn Fn() -> Instant>,
    ) -> Result<(), StoreFailure> {
        validate(binding, owner, refs)?;
        let mut conn = self.connection(deadline)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = clock.unwrap_or_else(unix_now);
        if expires_at <= now || expires_at > now.saturating_add(MAX_TTL_SECS) {
            return Err(StoreFailure::Unverified);
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
                return Err(StoreFailure::Unverified);
            }
            if previous.is_none() {
                if total >= MAX_ROWS || owned >= MAX_OWNER_ROWS {
                    return Err(StoreFailure::Unverified);
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
        if deadline.is_some_and(|deadline| Instant::now() >= deadline()) {
            return Err(StoreFailure::Busy);
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

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    #[test]
    fn queued_metadata_does_not_block_forever_on_the_connection_mutex_or_create_late_proof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.db");
        let store = Arc::new(ReferenceStore::open(path.to_str().unwrap()).unwrap());
        let held = store.conn.lock().unwrap();
        let worker = store.clone();
        let (tx, rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(40);
            let result = worker.issue_until(
                &"a".repeat(64),
                &"b".repeat(64),
                &[Reference {
                    kind: 1,
                    digest: "c".repeat(64),
                }],
                unix_now() + 60,
                || deadline,
            );
            tx.send(result.is_err()).unwrap();
        });
        let bounded = rx.recv_timeout(Duration::from_secs(1));
        drop(held);
        join.join().unwrap();
        assert_eq!(
            bounded,
            Ok(true),
            "a queued mutex wait must observe the original deadline"
        );
        assert!(store
            .authorize(
                &"a".repeat(64),
                &"b".repeat(64),
                &[Reference {
                    kind: 1,
                    digest: "c".repeat(64)
                }]
            )
            .is_err());
    }
}
