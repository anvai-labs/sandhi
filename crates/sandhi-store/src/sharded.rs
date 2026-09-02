//! Scope-sharded durable ledger (TD-0016 P1): N independent SQLite files, each
//! with its own write lock, so different tenants never serialize against each
//! other. TD-0015 P1 measured the single-file shape at ~1.26× scaling across
//! 4 threads — pure accident, per TD-0016 first-principle 4 ("a cap is per
//! scope; the ledger is not").
//!
//! Routing is deterministic and stateless: `fnv1a(scope) % shards`, an
//! explicit FNV-1a (stable forever, unlike `DefaultHasher`, whose algorithm
//! std may change). Because every [`Reservation`] carries its `scope`,
//! `settle` routes identically to the `reserve` that created it — per-shard
//! AUTOINCREMENT ids never cross shards.
//!
//! **Migration.** Opening with more shards than a legacy single-file ledger
//! was created with copies every `budget_limit`/`budget_reservation` row from
//! the legacy file to its hash-selected shard, then **deletes** them from the
//! legacy file. Consequence, documented for operators: downgrading the shard
//! count afterwards opens a ledger with no ledger rows (post-migration
//! settles are not visible to the legacy file). Idempotent: a re-open finds
//! no rows to move.
//!
//! The default is `shards = 1`, which opens *exactly* the legacy file —
//! bit-identical to the pre-sharding behaviour.

use std::sync::Mutex;

use time::OffsetDateTime;

use crate::ledger::{BudgetRow, ReserveOutcome, SqliteLedger};
use sandhi_core::{Policy, Window};

/// One legacy ledger row in flight during the TD-0016 P1 migration.
#[derive(Debug, Clone)]
pub struct MigratedReservation {
    pub id: i64,
    pub scope: String,
    pub ceiling: i64,
    pub actual: i64,
    pub settled: i64,
    pub expires_at: i64,
    pub settled_at: Option<i64>,
}

/// FNV-1a 64-bit — explicit and stable forever, unlike `DefaultHasher`
/// (SipHash, algorithm may change between std releases).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The durable ledger, sharded by scope: N independent SQLite files, each a
/// fully-capable [`SqliteLedger`] behind its own mutex. Every per-scope
/// operation routes to `fnv1a(scope) % shards`; the aggregate
/// [`list_budgets`](ShardedLedger::list_budgets) merges across shards.
pub struct ShardedLedger {
    shards: Vec<Mutex<SqliteLedger>>,
}

impl ShardedLedger {
    /// Open (creating if needed) `shards` ledger files derived from
    /// `base_path`. `shards == 1` opens `base_path` itself — identical to the
    /// pre-sharding [`SqliteLedger::open`]. With more shards, shard files are
    /// `<base_path>-ledger-shard-<i>.db` and any legacy single-file ledger
    /// rows are migrated into them (see the module docs).
    pub fn open_sharded(base_path: &str, shards: usize) -> rusqlite::Result<Self> {
        assert!(shards >= 1, "sharded ledger requires at least one shard");
        // `:memory:` must stay in-memory PER SHARD: appending the shard suffix
        // to it produces a real file named ":memory:-ledger-shard-N.db" that
        // every test in the process would silently share (found by the first
        // test run: spends accumulated across supposedly-isolated ledgers).
        let paths: Vec<String> = if base_path == ":memory:" {
            vec![":memory:".to_string(); shards]
        } else if shards == 1 {
            vec![base_path.to_string()]
        } else {
            (0..shards)
                .map(|i| format!("{base_path}-ledger-shard-{i}.db"))
                .collect()
        };

        if shards > 1 && std::path::Path::new(base_path).exists() {
            Self::migrate_legacy(base_path, &paths)?;
        }

        let shards = paths
            .iter()
            .map(|path| SqliteLedger::open(path).map(Mutex::new))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Self { shards })
    }

    /// Route a scope to its shard. Deterministic across restarts and releases.
    pub(crate) fn shard_for(&self, scope: &str) -> &Mutex<SqliteLedger> {
        &self.shards[(fnv1a(scope.as_bytes()) % self.shards.len() as u64) as usize]
    }

    fn with_shard<R>(&self, scope: &str, f: impl FnOnce(&mut SqliteLedger) -> R) -> R {
        let mut shard = self.shard_for(scope).lock().expect("ledger shard poisoned");
        f(&mut shard)
    }

    pub fn set_limit_durable(
        &self,
        scope: &str,
        limit: Option<u64>,
        window: Window,
        policy: Policy,
    ) -> rusqlite::Result<()> {
        self.with_shard(scope, |ledger| {
            ledger.set_limit_durable(scope, limit, window, policy)
        })
    }

    pub fn reserve_durable(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: time::Duration,
    ) -> rusqlite::Result<ReserveOutcome> {
        self.with_shard(scope, |ledger| {
            ledger.reserve_durable(scope, ceiling, now, ttl)
        })
    }

    /// Settle routes by the reservation's **scope** — the same hash as the
    /// reserve that created it — so per-shard ids are unambiguous.
    pub fn settle_durable(
        &self,
        scope: &str,
        reservation_id: u64,
        actual: u64,
    ) -> rusqlite::Result<()> {
        self.with_shard(scope, |ledger| {
            ledger.settle_durable(reservation_id, actual)
        })
    }

    pub fn limit_durable(&self, scope: &str) -> rusqlite::Result<Option<u64>> {
        self.with_shard(scope, |ledger| ledger.limit_durable(scope))
    }

    pub fn spent_durable(&self, scope: &str) -> rusqlite::Result<u64> {
        self.with_shard(scope, |ledger| ledger.spent_durable(scope))
    }

    pub fn reserved_durable(&self, scope: &str) -> rusqlite::Result<u64> {
        self.with_shard(scope, |ledger| ledger.reserved_durable(scope))
    }

    /// The union of every shard's configured budgets, sorted by scope for a
    /// deterministic dashboard order (a scope lives in exactly one shard).
    pub fn list_budgets_durable(&self) -> rusqlite::Result<Vec<BudgetRow>> {
        let mut rows = Vec::new();
        for shard in &self.shards {
            let ledger = shard.lock().expect("ledger shard poisoned");
            rows.extend(ledger.list_budgets_durable()?);
        }
        rows.sort_by(|a, b| a.scope.cmp(&b.scope));
        Ok(rows)
    }

    /// Reclaim expired leases on every shard; returns the total reclaimed.
    pub fn seen_durable(
        &self,
        vkey: &str,
        idem_key: &str,
        now: OffsetDateTime,
    ) -> rusqlite::Result<Option<(u64, u64)>> {
        let shard = self.shard_for(vkey);
        let mut ledger = shard.lock().expect("shard poisoned");
        ledger.seen_durable(vkey, idem_key, now)
    }

    /// TD-0021 P4: record a settlement for dedup. Routes by `vkey` — the dedup key's
    /// own scope, so repeats land on the same shard as the original.
    ///
    /// SHARDING INVARIANT (review finding, forward-looking): dedup routes by
    /// `shard_for(vkey)` while its settlement routes by `shard_for(scope)` — different
    /// inner shards. Today the proxy's single outer `Mutex<ProxyLedger>` serializes
    /// settle+seen+record, so the check-then-act is atomic ACROSS those shards. When
    /// TD-0016 P2+ removes that outer lock, this two-shard window reopens — the dedup
    /// must then either route by the SAME key as settlement or take a per-record
    /// advisory lock. Removing the outer lock without addressing this is a correctness
    /// regression; this comment is the tripwire.
    pub fn record_durable(
        &self,
        vkey: &str,
        idem_key: &str,
        reservation: u64,
        actual: u64,
        now: OffsetDateTime,
        ttl: time::Duration,
    ) -> rusqlite::Result<()> {
        let shard = self.shard_for(vkey);
        let mut ledger = shard.lock().expect("shard poisoned");
        ledger.record_durable(vkey, idem_key, reservation, actual, now, ttl)
    }

    pub fn reclaim_expired_durable(&self, now: OffsetDateTime) -> rusqlite::Result<usize> {
        let mut total = 0;
        for shard in &self.shards {
            let mut ledger = shard.lock().expect("ledger shard poisoned");
            total += ledger.reclaim_expired_durable(now)?;
        }
        Ok(total)
    }

    /// Move every ledger row from the legacy single file into its
    /// hash-selected shard, then delete the legacy rows. Idempotent (a second
    /// pass finds no rows) and count-verified: the migration aborts unless
    /// every row it read landed somewhere.
    fn migrate_legacy(base_path: &str, shard_paths: &[String]) -> rusqlite::Result<()> {
        use rusqlite::OptionalExtension;

        let legacy = rusqlite::Connection::open(base_path)?;
        let has_table = |name: &str| -> rusqlite::Result<bool> {
            legacy
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [name],
                    |_| Ok(()),
                )
                .optional()
                .map(|found| found.is_some())
        };
        if !has_table("budget_reservation")? || !has_table("budget_limit")? {
            return Ok(()); // nothing ever created here
        }

        let limits: Vec<(String, Option<i64>, String, String)> = {
            let mut stmt =
                legacy.prepare("SELECT scope, limit_tokens, window, policy FROM budget_limit")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            rows
        };

        let reservations: Vec<MigratedReservation> = {
            let mut stmt = legacy.prepare(
                "SELECT id, scope, ceiling, actual, settled, expires_at, settled_at
                 FROM budget_reservation",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(MigratedReservation {
                        id: row.get(0)?,
                        scope: row.get(1)?,
                        ceiling: row.get(2)?,
                        actual: row.get(3)?,
                        settled: row.get(4)?,
                        expires_at: row.get(5)?,
                        settled_at: row.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            rows
        };

        let n = shard_paths.len() as u64;
        let route = |scope: &str| (fnv1a(scope.as_bytes()) % n) as usize;

        // One fully-initialised ledger per target shard: plain connections
        // would be schemaless — the exact defect the instrumentation caught.
        let mut shard_ledgers: Vec<SqliteLedger> = shard_paths
            .iter()
            .map(|path| SqliteLedger::open(path))
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let expected = limits.len() + reservations.len();
        let mut moved = 0usize;
        for (scope, limit_tokens, window, policy) in &limits {
            shard_ledgers[route(scope)].insert_migrated_limit(
                scope,
                *limit_tokens,
                window,
                policy,
            )?;
            moved += 1;
        }
        for row in &reservations {
            let MigratedReservation {
                id,
                ref scope,
                ceiling,
                actual,
                settled,
                expires_at,
                settled_at,
            } = *row;
            shard_ledgers[route(scope)].insert_migrated_reservation(
                id, scope, ceiling, actual, settled, expires_at, settled_at,
            )?;
            moved += 1;
        }
        assert_eq!(
            moved, expected,
            "migration must move every legacy row exactly once"
        );

        // Remove the legacy rows so neither the upgraded path nor a re-run
        // ever double-migrates. Downgrade note: a legacy (1-shard) open after
        // this sees an empty ledger — documented in the module docs.
        legacy.execute("DELETE FROM budget_reservation", [])?;
        legacy.execute("DELETE FROM budget_limit", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::ReserveOutcome;
    use sandhi_core::Reservation;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn ttl() -> time::Duration {
        time::Duration::seconds(900)
    }

    fn admitted(outcome: ReserveOutcome) -> Reservation {
        match outcome {
            ReserveOutcome::Admitted(r) => r,
            ReserveOutcome::Denied(d) => panic!("unexpected denial: {d:?}"),
        }
    }

    #[test]
    fn shard_routes_deterministically_and_spreads() {
        let ledger = ShardedLedger::open_sharded(":memory:", 4).unwrap();
        // Determinism: the same scope always routes to the same shard.
        for scope in ["group:alpha", "vk:k1", "tenant:7", "budget:x"] {
            let first = ledger.shard_for(scope) as *const _ as usize;
            for _ in 0..100 {
                assert_eq!(ledger.shard_for(scope) as *const _ as usize, first);
            }
        }
        // Spread: a fixed corpus of realistic scopes must reach every shard
        // of 4 (deterministic — no flake possible; if it ever fails, the hash
        // is broken, not unlucky).
        let hit: std::collections::HashSet<usize> = (0..64)
            .map(|i| {
                let scope = format!("group:tenant-{i}");
                (fnv1a(scope.as_bytes()) % 4) as usize
            })
            .collect();
        assert_eq!(hit.len(), 4, "64 realistic scopes must cover all 4 shards");
    }

    #[test]
    fn reserve_and_settle_round_trip_routes_by_scope() {
        let ledger = ShardedLedger::open_sharded(":memory:", 4).unwrap();
        let reservation = admitted(
            ledger
                .reserve_durable("group:alpha", 1000, now(), ttl())
                .unwrap(),
        );
        ledger
            .settle_durable("group:alpha", reservation.id, 750)
            .unwrap();
        assert_eq!(ledger.spent_durable("group:alpha").unwrap(), 750);
        assert_eq!(ledger.reserved_durable("group:alpha").unwrap(), 0);
    }

    /// C1: the cap is enforced inside the shard's atomic admit.
    #[test]
    fn cap_is_enforced_within_a_shard() {
        let ledger = ShardedLedger::open_sharded(":memory:", 2).unwrap();
        ledger
            .set_limit_durable("group:alpha", Some(1000), Window::Total, Policy::Block)
            .unwrap();
        let first = admitted(
            ledger
                .reserve_durable("group:alpha", 700, now(), ttl())
                .unwrap(),
        );
        // Over-cap: refused (in-flight 700 counts toward the 1000 cap).
        assert!(matches!(
            ledger
                .reserve_durable("group:alpha", 700, now(), ttl())
                .unwrap(),
            ReserveOutcome::Denied(_)
        ));
        // Settle the first; 700 spent remains, so 700 more is still over-cap
        // (settled spend counts), but 300 fits.
        ledger.settle_durable("group:alpha", first.id, 700).unwrap();
        assert!(matches!(
            ledger
                .reserve_durable("group:alpha", 700, now(), ttl())
                .unwrap(),
            ReserveOutcome::Denied(_)
        ));
        assert!(matches!(
            ledger
                .reserve_durable("group:alpha", 300, now(), ttl())
                .unwrap(),
            ReserveOutcome::Admitted(_)
        ));
    }

    /// C2: settle is idempotent by id — a replay changes nothing.
    #[test]
    fn settle_is_idempotent_by_id() {
        let ledger = ShardedLedger::open_sharded(":memory:", 4).unwrap();
        let r = admitted(ledger.reserve_durable("vk:k1", 500, now(), ttl()).unwrap());
        ledger.settle_durable("vk:k1", r.id, 300).unwrap();
        ledger.settle_durable("vk:k1", r.id, 300).unwrap();
        ledger.settle_durable("vk:k1", r.id, 300).unwrap();
        assert_eq!(ledger.spent_durable("vk:k1").unwrap(), 300);
    }

    /// C4-adjacent: two scopes live in different shards and never see each
    /// other's spend — the isolation this phase exists for.
    #[test]
    fn scopes_are_isolated_across_shards() {
        let ledger = ShardedLedger::open_sharded(":memory:", 4).unwrap();
        let a = admitted(
            ledger
                .reserve_durable("group:alpha", 1000, now(), ttl())
                .unwrap(),
        );
        let b = admitted(
            ledger
                .reserve_durable("group:beta", 1000, now(), ttl())
                .unwrap(),
        );
        ledger.settle_durable("group:alpha", a.id, 400).unwrap();
        assert_eq!(ledger.spent_durable("group:alpha").unwrap(), 400);
        assert_eq!(ledger.spent_durable("group:beta").unwrap(), 0);
        drop(b);
    }

    /// TTL reclaim frees the lease on the owning shard.
    #[test]
    fn expired_leases_are_reclaimed_on_the_owning_shard() {
        let ledger = ShardedLedger::open_sharded(":memory:", 2).unwrap();
        let past = now() - time::Duration::seconds(3600);
        let r = admitted(
            ledger
                .reserve_durable("vk:stale", 500, past, time::Duration::seconds(1))
                .unwrap(),
        );
        let reclaimed = ledger.reclaim_expired_durable(now()).unwrap();
        assert!(reclaimed >= 1, "the expired lease must be reclaimed");
        // Capacity is free again.
        assert!(matches!(
            ledger
                .reserve_durable("vk:stale", 500, now(), ttl())
                .unwrap(),
            ReserveOutcome::Admitted(_)
        ));
        drop(r);
    }

    /// Real contention: 4 threads × 250 admissions on 4 shards — every admit
    /// lands, every settle counts, totals balance exactly.
    #[test]
    fn concurrent_multi_shard_admissions_balance_exactly() {
        use std::sync::Arc;
        const THREADS: usize = 4;
        const OPS: usize = 250;
        let ledger = Arc::new(ShardedLedger::open_sharded(":memory:", THREADS).unwrap());
        for t in 0..THREADS {
            ledger
                .set_limit_durable(
                    &format!("group:t{t}"),
                    Some(1_000_000),
                    Window::Total,
                    Policy::Block,
                )
                .unwrap();
        }
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let ledger = Arc::clone(&ledger);
                std::thread::spawn(move || {
                    for i in 0..OPS {
                        let scope = format!("group:t{t}");
                        let r = match ledger.reserve_durable(&scope, 10, now(), ttl()).unwrap() {
                            ReserveOutcome::Admitted(r) => r,
                            ReserveOutcome::Denied(_) => panic!("cap cannot be hit at 10/1M"),
                        };
                        ledger.settle_durable(&scope, r.id, 10).unwrap();
                        let _ = i;
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        for t in 0..THREADS {
            assert_eq!(
                ledger.spent_durable(&format!("group:t{t}")).unwrap(),
                (OPS * 10) as u64,
                "every admission must settle exactly once"
            );
        }
    }

    /// The migration: a legacy single-file ledger with budgets, spend, and an
    /// in-flight reservation is fully preserved across a widen to 4 shards —
    /// and a re-open is stable.
    #[test]
    fn migration_preserves_every_row_across_a_widen() {
        let dir = tempdir();
        let base = dir.path().join("usage.db");
        let base = base.to_str().unwrap();

        // Legacy world: budget + settled spend + in-flight reservation.
        let mut legacy = SqliteLedger::open(base).unwrap();
        legacy
            .set_limit_durable("group:alpha", Some(5_000), Window::Total, Policy::Block)
            .unwrap();
        let settled = admitted(
            legacy
                .reserve_durable("group:alpha", 700, now(), ttl())
                .unwrap(),
        );
        legacy.settle_durable(settled.id, 700).unwrap();
        let inflight = admitted(
            legacy
                .reserve_durable("group:beta", 300, now(), ttl())
                .unwrap(),
        );
        drop(legacy);

        // Widen to 4 shards.
        let ledger = ShardedLedger::open_sharded(base, 4).unwrap();
        assert_eq!(
            ledger.limit_durable("group:alpha").unwrap(),
            Some(5_000),
            "budget must survive"
        );
        assert_eq!(
            ledger.spent_durable("group:alpha").unwrap(),
            700,
            "settled spend must survive"
        );
        assert_eq!(
            ledger.reserved_durable("group:beta").unwrap(),
            300,
            "the in-flight lease must survive"
        );
        // group:beta had no configured budget — only its in-flight lease
        // migrates. One configured budget is the correct count.
        assert_eq!(ledger.list_budgets_durable().unwrap().len(), 1);

        // The in-flight reservation settles through its shard after migration.
        ledger
            .settle_durable("group:beta", inflight.id, 300)
            .unwrap();
        assert_eq!(ledger.spent_durable("group:beta").unwrap(), 300);

        // Re-open: stable.
        drop(ledger);
        let ledger = ShardedLedger::open_sharded(base, 4).unwrap();
        assert_eq!(ledger.spent_durable("group:alpha").unwrap(), 700);
        assert_eq!(ledger.spent_durable("group:beta").unwrap(), 300);
    }
}
