//! Postgres native-reference ownership store — the PG port of
//! [`crate::references::ReferenceStore`]. Digest-only issuer proofs; no prompt, native
//! payload, provider ID or credential is stored. The store reuses the SQLite module's
//! error type ([`crate::references::Error`], a unit "unavailable or unverified" error)
//! and its constants verbatim.
use std::time::{Duration, Instant};

use super::{init_ddl, OperatorPool};
pub use crate::references::{
    Error, Reference, ENCRYPTED_REASONING, MAX_BATCH, MAX_OWNER_ROWS, MAX_ROWS, MAX_TTL_SECS,
    REDACTED_THINKING, THINKING_SIGNATURE,
};

fn self_contained(kind: u8) -> bool {
    matches!(
        kind,
        ENCRYPTED_REASONING | THINKING_SIGNATURE | REDACTED_THINKING
    )
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

/// Idempotent schema init, mirroring the SQLite DDL column-for-column, with ONE recorded
/// addition: an explicit `id bigserial` insertion-order key replaces SQLite's implicit
/// `rowid`, which the bounded cleanup deletes order by (SQLite schema is NOT changed).
/// The four-column ownership key stays UNIQUE so `ON CONFLICT` keeps the same conflict
/// target; the single-owner partial unique index and both indexes translate verbatim.
/// (The SQLite `open` re-checks the primary-key ordinal via `pragma_table_info` as a
/// legacy-schema guard; a Postgres store is born with this exact shape — recorded
/// decision, mirroring the ledger core.)
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS issued_native_references (
        id bigserial PRIMARY KEY,
        binding text NOT NULL CHECK(length(binding)=64),
        kind smallint NOT NULL CHECK(kind BETWEEN 1 AND 16),
        reference_digest text NOT NULL CHECK(length(reference_digest)=64),
        owner text NOT NULL CHECK(length(owner)=64),
        expires_at bigint NOT NULL,
        UNIQUE(binding,kind,reference_digest,owner)
    )",
    // A resource ID has ONE owner; self-contained payloads (kinds 3/6/7) may be
    // independently issued to multiple owners. Same predicate, same name.
    "CREATE UNIQUE INDEX IF NOT EXISTS native_resource_single_owner \
         ON issued_native_references(binding,kind,reference_digest) WHERE kind NOT IN (3,6,7)",
    "CREATE INDEX IF NOT EXISTS native_references_expiry ON issued_native_references(expires_at)",
    "CREATE INDEX IF NOT EXISTS native_references_owner ON issued_native_references(owner)",
];

/// A Postgres-backed native-reference ownership store.
pub struct PostgresReferenceStore {
    pool: OperatorPool,
}

impl PostgresReferenceStore {
    /// Connect and initialize the schema (idempotent `CREATE IF NOT EXISTS`).
    ///
    /// Must be called on a `tokio` runtime; that runtime's handle is retained for every
    /// later blocking statement.
    /// Like [`Self::connect`] from pre-parsed connect options: the startup wiring sets
    /// `application_name` (and the test suite points `search_path` at a per-test schema).
    pub async fn connect_with(options: sqlx::postgres::PgConnectOptions) -> Result<Self, sqlx::Error> {
        let pool = OperatorPool::connect_with(options, 2).await?;
        init_ddl(pool.pool(), DDL).await?;
        Ok(Self { pool })
    }

    /// Open the store on `dsn` and initialize the schema (idempotent).
    pub async fn connect(dsn: &str) -> Result<Self, sqlx::Error> {
        let options: sqlx::postgres::PgConnectOptions = dsn
            .parse()
            .map_err(|e| sqlx::Error::Configuration(format!("invalid DSN: {e}").into()))?;
        Self::connect_with(options).await
    }

    /// All references must be positively issued to this exact scope. Expired rows grant
    /// nothing.
    pub fn authorize(&self, binding: &str, owner: &str, refs: &[Reference]) -> Result<(), Error> {
        self.authorize_at(binding, owner, refs, None)
    }

    /// Retry only the identical metadata lookup on transient contention. The caller
    /// supplies its original deadline, including any later shutdown cut.
    pub fn authorize_until(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        deadline: impl Fn() -> Instant,
    ) -> Result<(), Error> {
        self.until(&deadline, || {
            self.authorize_inner(binding, owner, refs, None)
        })
    }

    fn authorize_at(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
    ) -> Result<(), Error> {
        self.authorize_inner(binding, owner, refs, clock)
            .map_err(|_| Error)
    }

    fn authorize_inner(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
    ) -> Result<(), Attempt> {
        validate(binding, owner, refs).map_err(|_| Attempt::Fatal(Error))?;
        self.block(async {
            let pool = self.pool.pool();
            for r in refs {
                let expires_at: Option<i64> = sqlx::query_scalar(
                    "SELECT expires_at FROM issued_native_references \
                     WHERE binding=$1 AND kind=$2 AND reference_digest=$3 AND owner=$4",
                )
                .bind(binding)
                .bind(i16::from(r.kind))
                .bind(&r.digest)
                .bind(owner)
                .fetch_optional(pool)
                .await
                .map_err(Attempt::from)?;
                if !expires_at.is_some_and(|expiry| expiry > clock.unwrap_or_else(unix_now)) {
                    return Err(Attempt::Fatal(Error));
                }
            }
            Ok(())
        })
    }

    /// A provider acknowledgement may add proof, never transfer another live owner's
    /// proof. One complete frame/buffer commits atomically before its bytes are forwarded.
    pub fn issue(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        expires_at: i64,
    ) -> Result<(), Error> {
        self.issue_at(binding, owner, refs, None, expires_at)
    }

    /// Contention retries never change the owner, reference batch or expiry and never
    /// expose bytes before the original atomic proof transaction commits.
    pub fn issue_until(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        expires_at: i64,
        deadline: impl Fn() -> Instant,
    ) -> Result<(), Error> {
        self.until(&deadline, || {
            self.issue_inner(binding, owner, refs, None, expires_at)
        })
    }

    fn issue_at(
        &self,
        binding: &str,
        owner: &str,
        refs: &[Reference],
        clock: Option<i64>,
        expires_at: i64,
    ) -> Result<(), Error> {
        self.issue_inner(binding, owner, refs, clock, expires_at)
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
    ) -> Result<(), Attempt> {
        validate(binding, owner, refs).map_err(|_| Attempt::Fatal(Error))?;
        self.block(async {
            let mut tx = self.pool.pool().begin().await.map_err(Attempt::from)?;
            // SQLite's BEGIN IMMEDIATE for the proof transaction: one advisory lock holds
            // the bounded-cleanup → capacity-count → insert sequence across pool
            // connections.
            super::store_lock(&mut tx, "issued_native_references")
                .await
                .map_err(Attempt::from)?;
            let now = clock.unwrap_or_else(unix_now);
            if expires_at <= now || expires_at > now.saturating_add(MAX_TTL_SECS) {
                return Err(Attempt::Fatal(Error));
            }
            // Cleanup is bounded; capacity failure is fail closed, never an unbounded
            // sweep. Reserve half the cleanup budget for this owner so a global backlog
            // cannot leave its expired 4096-row quota permanently full on rollback.
            // (`id` replaces SQLite's `rowid` — same ordering semantics.)
            let mut cleaned = sqlx::query(
                "DELETE FROM issued_native_references WHERE id IN \
                 (SELECT id FROM issued_native_references WHERE owner=$1 AND expires_at<=$2 \
                  ORDER BY expires_at LIMIT 128)",
            )
            .bind(owner)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(Attempt::from)?
            .rows_affected() as usize;
            // Fresh source issuance can replace an expired resource-ID proof even when
            // its row is behind the bounded global oldest-first window. Authorization
            // never performs this deletion or adopts caller input.
            for reference in refs.iter().filter(|r| !self_contained(r.kind)) {
                cleaned += sqlx::query(
                    "DELETE FROM issued_native_references \
                     WHERE binding=$1 AND kind=$2 AND reference_digest=$3 AND expires_at<=$4",
                )
                .bind(binding)
                .bind(i16::from(reference.kind))
                .bind(&reference.digest)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(Attempt::from)?
                .rows_affected() as usize;
            }
            let remaining = 256usize.saturating_sub(cleaned);
            sqlx::query(
                "DELETE FROM issued_native_references WHERE id IN \
                 (SELECT id FROM issued_native_references WHERE expires_at<=$1 \
                  ORDER BY expires_at LIMIT $2)",
            )
            .bind(now)
            .bind(remaining as i64)
            .execute(&mut *tx)
            .await
            .map_err(Attempt::from)?;
            let mut total: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM issued_native_references")
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(Attempt::from)?;
            let mut owned: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM issued_native_references WHERE owner=$1")
                    .bind(owner)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(Attempt::from)?;
            for r in refs {
                // Self-contained payloads may be independently issued to multiple owners.
                // A resource ID has one owner; an ambiguous collision refuses.
                let previous: Option<(String, i64)> = if self_contained(r.kind) {
                    sqlx::query_as(
                        "SELECT owner,expires_at FROM issued_native_references \
                         WHERE binding=$1 AND kind=$2 AND reference_digest=$3 AND owner=$4",
                    )
                    .bind(binding)
                    .bind(i16::from(r.kind))
                    .bind(&r.digest)
                    .bind(owner)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(Attempt::from)?
                } else {
                    sqlx::query_as(
                        "SELECT owner,expires_at FROM issued_native_references \
                         WHERE binding=$1 AND kind=$2 AND reference_digest=$3",
                    )
                    .bind(binding)
                    .bind(i16::from(r.kind))
                    .bind(&r.digest)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(Attempt::from)?
                };
                if previous
                    .as_ref()
                    .is_some_and(|(old_owner, _)| old_owner != owner)
                {
                    return Err(Attempt::Fatal(Error));
                }
                if previous.is_none() {
                    if total >= MAX_ROWS || owned >= MAX_OWNER_ROWS {
                        return Err(Attempt::Fatal(Error));
                    }
                    total += 1;
                    owned += 1;
                }
                sqlx::query(
                    "INSERT INTO issued_native_references \
                     (binding,kind,reference_digest,owner,expires_at) VALUES ($1,$2,$3,$4,$5) \
                     ON CONFLICT(binding,kind,reference_digest,owner) DO UPDATE SET \
                     expires_at=GREATEST(issued_native_references.expires_at,excluded.expires_at)",
                )
                .bind(binding)
                .bind(i16::from(r.kind))
                .bind(&r.digest)
                .bind(owner)
                .bind(expires_at)
                .execute(&mut *tx)
                .await
                .map_err(Attempt::from)?;
            }
            tx.commit().await.map_err(Attempt::from)
        })
    }

    /// The identical-attempt retry loop of the SQLite twin: retry only transient failures
    /// within the caller's original deadline, then report busy/unverified as one opaque
    /// [`Error`].
    fn until(
        &self,
        deadline: &dyn Fn() -> Instant,
        mut attempt: impl FnMut() -> Result<(), Attempt>,
    ) -> Result<(), Error> {
        let mut contended = false;
        loop {
            if Instant::now() >= deadline() {
                return Err(Error);
            }
            match attempt() {
                Ok(()) => return Ok(()),
                Err(Attempt::Retry) => {
                    if !contended {
                        tracing::debug!(
                            reason = "retrying",
                            "retrying identical native reference metadata within original deadline"
                        );
                        contended = true;
                    }
                    std::thread::sleep(
                        Duration::from_millis(1)
                            .min(deadline().saturating_duration_since(Instant::now())),
                    );
                }
                Err(Attempt::Fatal(error)) => return Err(error),
            }
        }
    }

    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.pool.block(fut)
    }
}

/// Retry-lane classification for the native-reference retry loop: pool/network
/// failures retry within the caller deadline; constraint violations, validation
/// failures and decode errors are deterministic and surface immediately.
enum Attempt {
    Retry,
    Fatal(Error),
}

impl From<sqlx::Error> for Attempt {
    fn from(error: sqlx::Error) -> Self {
        match error {
            // Transient contention/transport: pool timed out acquiring a connection,
            // or the wire dropped mid-statement. Retrying within the deadline is sound.
            sqlx::Error::PoolTimedOut | sqlx::Error::Io(_) => Self::Retry,
            // Deterministic: constraint/validation/decode failures repeat identically.
            _ => Self::Fatal(Error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg_refs(tag: &str) -> (String, PostgresReferenceStore) {
        super::super::pg_test::connect_in_schema(tag, PostgresReferenceStore::connect_with)
    }

    fn drop_schema(schema: &str) {
        super::super::pg_test::drop_schema_best_effort(schema)
    }

    fn reference(kind: u8, nibble: u8) -> Reference {
        Reference {
            kind,
            digest: format!("{nibble:064x}"),
        }
    }

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn ddl_pins_the_single_owner_partial_unique_index_and_explicit_id_key() {
        let joined = DDL.join("\n");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS issued_native_references"));
        // The explicit insertion-order key that replaces SQLite's implicit rowid.
        assert!(joined.contains("id bigserial PRIMARY KEY"));
        assert!(joined.contains("UNIQUE(binding,kind,reference_digest,owner)"));
        // The single-owner partial unique index keeps its exact name and predicate.
        assert!(
            DDL.iter()
                .any(|s| s.contains("native_resource_single_owner")
                    && s.contains("WHERE kind NOT IN (3,6,7)")),
            "{joined}"
        );
        assert!(joined.contains("native_references_expiry ON issued_native_references(expires_at)"));
        assert!(joined.contains("native_references_owner ON issued_native_references(owner)"));
        // SQLite's schema is NOT changed: no `id` column exists in the SQLite DDL.
        assert!(!joined.contains("rowid"));
        assert!(!joined.contains("INSERT OR"));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn single_owner_partial_unique_enforcement_and_expiry() {
        let (schema, store) = pg_refs("refs-owner");
        let binding = "c".repeat(64);
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let r1 = reference(1, 0xd);

        // Unissued → authorize refuses.
        assert!(store
            .authorize_at(&binding, &a, std::slice::from_ref(&r1), Some(10))
            .is_err());
        store
            .issue_at(&binding, &a, std::slice::from_ref(&r1), Some(10), 20)
            .unwrap();
        // THE single-owner invariant: a second live owner for a resource ID (kind 1,
        // covered by the partial unique index) is refused.
        assert!(store
            .issue_at(&binding, &b, std::slice::from_ref(&r1), Some(11), 21)
            .is_err());
        // Expiry bounds the grant on both sides.
        assert!(store
            .authorize_at(&binding, &a, std::slice::from_ref(&r1), Some(19))
            .is_ok());
        assert!(store
            .authorize_at(&binding, &b, std::slice::from_ref(&r1), Some(19))
            .is_err());
        assert!(store
            .authorize_at(&"d".repeat(64), &a, std::slice::from_ref(&r1), Some(19))
            .is_err());
        assert!(store
            .authorize_at(&binding, &a, std::slice::from_ref(&r1), Some(20))
            .is_err());

        // A batch containing an unissued reference fails atomically.
        let next = reference(2, 0xe);
        assert!(store
            .issue_at(&binding, &b, &[next.clone(), r1.clone()], Some(19), 30)
            .is_err());
        assert!(store.authorize_at(&binding, &b, &[next], Some(19)).is_err());

        // A fresh issue after expiry replaces the lapsed proof.
        store
            .issue_at(&binding, &b, std::slice::from_ref(&r1), Some(21), 30)
            .unwrap();
        assert!(store
            .authorize_at(&binding, &a, std::slice::from_ref(&r1), Some(21))
            .is_err());
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn self_contained_kinds_independently_issue_to_multiple_owners() {
        let (schema, store) = pg_refs("refs-selfcontained");
        let binding = "a".repeat(64);
        let owner_a = "b".repeat(64);
        let owner_b = "c".repeat(64);
        for kind in [ENCRYPTED_REASONING, THINKING_SIGNATURE, REDACTED_THINKING] {
            let r = reference(kind, 0xd);
            store
                .issue_at(&binding, &owner_a, std::slice::from_ref(&r), Some(10), 20)
                .unwrap();
            assert!(store
                .authorize_at(&binding, &owner_b, std::slice::from_ref(&r), Some(11))
                .is_err());
            store
                .issue_at(&binding, &owner_b, std::slice::from_ref(&r), Some(11), 21)
                .unwrap();
            for owner in [&owner_a, &owner_b] {
                store
                    .authorize_at(&binding, owner, std::slice::from_ref(&r), Some(12))
                    .unwrap();
            }
        }
        // The opaque kind-1 proof still refuses transfer.
        let r1 = reference(1, 0xd);
        store
            .issue_at(&binding, &owner_a, std::slice::from_ref(&r1), Some(10), 20)
            .unwrap();
        assert!(store
            .issue_at(&binding, &owner_b, std::slice::from_ref(&r1), Some(11), 21)
            .is_err());
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn bounded_expiry_cleanup_and_capacity_refuse_fail_closed() {
        let (schema, store) = pg_refs("refs-capacity");
        let binding = "b".repeat(64);
        let owner = "a".repeat(64);
        let fresh = reference(1, 0xc);

        // Batch and TTL bounds refuse without writing anything.
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

        // Fill this owner's full quota with rows not yet expired at clock 10.
        store.block(async {
            sqlx::query(
                "INSERT INTO issued_native_references \
                 (binding, kind, reference_digest, owner, expires_at) \
                 SELECT $1, $2, lpad(x::text, 64, '0'), $3, $4 \
                 FROM generate_series(1, $5) AS g(x)",
            )
            .bind(&binding)
            .bind(i16::from(1_u8))
            .bind(&owner)
            .bind(20_i64)
            .bind(MAX_OWNER_ROWS)
            .execute(store.pool.pool())
            .await
            .expect("seed owner quota");
        });
        assert!(store
            .issue_at(&binding, &owner, std::slice::from_ref(&fresh), Some(10), 20)
            .is_err());
        // The owner's own expired row is refreshable (same owner, same conflict target).
        let existing = reference(1, 1);
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

        // All rows are expired, but other owners' rows expire earlier. Global
        // oldest-first cleanup alone cannot free this owner's full quota — the targeted
        // per-owner sweep does (exactly the SQLite test's final phase).

        // Seed to the global MAX_ROWS cap across many owners, expired at 15 (the PG
        // dialect of the SQLite tests' recursive INSERT).
        store.block(async {
            sqlx::query(
                "INSERT INTO issued_native_references \
                 (binding, kind, reference_digest, owner, expires_at) \
                 SELECT $1, 1, lpad(x::text, 64, '0'), lpad((x/4000)::text, 64, '0'), 15 \
                 FROM generate_series($2, $3) AS g(x)",
            )
            .bind(&binding)
            .bind(MAX_OWNER_ROWS + 1)
            .bind(MAX_ROWS)
            .execute(store.pool.pool())
            .await
            .expect("seed global cap");
        });
        assert!(store
            .issue_at(
                &binding,
                &"d".repeat(64),
                std::slice::from_ref(&fresh),
                Some(10),
                20
            )
            .is_err());
        let count = |store: &PostgresReferenceStore| -> i64 {
            store.block(async {
                sqlx::query_scalar("SELECT count(*) FROM issued_native_references")
                    .fetch_one(store.pool.pool())
                    .await
                    .unwrap()
            })
        };
        assert_eq!(count(&store), MAX_ROWS);
        store
            .issue_at(&binding, &owner, std::slice::from_ref(&fresh), Some(21), 30)
            .unwrap();
        store
            .authorize_at(&binding, &owner, &[fresh], Some(21))
            .unwrap();
        assert_eq!(count(&store), MAX_ROWS - 255);
        drop_schema(&schema);
    }
}
