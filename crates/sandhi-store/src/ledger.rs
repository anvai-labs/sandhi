//! Durable, crash-safe enforcement ledger (ADR-0005 D2/D5).
//!
//! A SQLite-backed implementation of the [`sandhi_core::EnforcementLedger`] / [`LedgerView`]
//! contract. It carries the three properties that must co-exist for a durable ledger to be *more*
//! correct than the in-memory one, not less (ADR-0005 Context #3):
//!
//! - **TTL leases** — [`reserve`](SqliteLedger::reserve_durable) persists a reservation row; a lease
//!   left dangling by a crash is reclaimed ([`reclaim_expired`](SqliteLedger::reclaim_expired_durable),
//!   plus opportunistic reclaim inside `reserve`) rather than leaking capacity forever (C1).
//! - **Idempotent settle by id** — [`settle`](SqliteLedger::settle_durable) is a
//!   `reserved → settled` transition guarded by `settled = 0`; a repeat (at-least-once delivery)
//!   updates zero rows and is a no-op (C2).
//! - **Atomic conditional reserve in the store** — the admit is a single `BEGIN IMMEDIATE`
//!   transaction: reclaim-then-read-then-check-then-insert, all under SQLite's write lock, so two
//!   callers cannot both admit against a stale read (C3). Never SELECT-in-caller then UPDATE.
//!
//! The inherent `*_durable` methods return `rusqlite::Result` so a caller can apply a per-tier
//! fail-open/closed policy (ADR-0005 D6). The trait impls below are thin **fail-closed** adapters
//! (a backend error denies / reads as empty) for the simple swap; richer policy + calendar windows
//! + a Redis HA backend are tracked follow-ups. Neutral tokens only — no dollars.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use time::{Duration, OffsetDateTime};

use sandhi_core::{Denied, EnforcementLedger, LedgerView, Policy, Reservation, Window};

/// Result of an atomic reserve: admitted (with the lease) or denied (over cap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    Admitted(Reservation),
    Denied(Denied),
}

/// A configured budget row — the durable cap + window + policy for a scope. Read back on startup so
/// the operator's in-memory budget metadata (policy lookup, dashboard, alert thresholds) is
/// rehydrated from the durable ledger rather than lost on restart (ADR-0005 D3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetRow {
    pub scope: String,
    pub limit: Option<u64>,
    pub window: Window,
    pub policy: Policy,
}

/// A durable enforcement ledger backed by a SQLite connection. `&mut self` on the mutating methods
/// gives exclusive access (the proxy shares one behind a `Mutex`), so no interior lock is needed.
pub struct SqliteLedger {
    conn: Connection,
}

impl SqliteLedger {
    /// Open (creating if needed) a ledger at `path` (`:memory:` for a volatile one).
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(&conn)?;
        Ok(Self { conn })
    }

    fn init(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS budget_limit (
                 scope        TEXT PRIMARY KEY,
                 limit_tokens INTEGER,
                 window       TEXT NOT NULL DEFAULT 'total',
                 policy       TEXT NOT NULL DEFAULT 'block'
             );
             CREATE TABLE IF NOT EXISTS budget_reservation (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope      TEXT    NOT NULL,
                 ceiling    INTEGER NOT NULL,
                 actual     INTEGER NOT NULL DEFAULT 0,
                 settled    INTEGER NOT NULL DEFAULT 0,
                 settled_at INTEGER,
                 expires_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_reservation_scope
                 ON budget_reservation (scope, settled);",
        )
    }

    /// The inclusive start (unix seconds) of the current window for `window`, calendar-aligned in
    /// UTC (the store is the clock authority, ADR-0005 D5). `Total` returns 0 (all-time).
    fn window_start_ts(now: OffsetDateTime, window: Window) -> i64 {
        match window {
            Window::Total => 0,
            Window::Daily => now.replace_time(time::Time::MIDNIGHT).unix_timestamp(),
            Window::Monthly => now
                .replace_day(1)
                .unwrap_or(now)
                .replace_time(time::Time::MIDNIGHT)
                .unix_timestamp(),
        }
    }

    /// Set (or clear the cap with `limit = None`) a scope's durable budget: cap + window + policy.
    pub fn set_limit_durable(
        &mut self,
        scope: &str,
        limit: Option<u64>,
        window: Window,
        policy: Policy,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO budget_limit (scope, limit_tokens, window, policy) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope) DO UPDATE SET
                 limit_tokens = excluded.limit_tokens,
                 window = excluded.window,
                 policy = excluded.policy",
            params![
                scope,
                limit.map(|v| v as i64),
                window.as_str(),
                policy.as_str()
            ],
        )?;
        Ok(())
    }

    /// The configured window for a scope (defaults to `Total` when unset).
    pub fn window_of_durable(&self, scope: &str) -> rusqlite::Result<Window> {
        let w: Option<String> = self
            .conn
            .query_row(
                "SELECT window FROM budget_limit WHERE scope = ?1",
                [scope],
                |row| row.get(0),
            )
            .optional()?;
        Ok(w.map(|s| Window::parse(&s)).unwrap_or_default())
    }

    /// The configured policy for a scope (defaults to `Block` when unset).
    pub fn policy_of_durable(&self, scope: &str) -> rusqlite::Result<Policy> {
        let p: Option<String> = self
            .conn
            .query_row(
                "SELECT policy FROM budget_limit WHERE scope = ?1",
                [scope],
                |row| row.get(0),
            )
            .optional()?;
        Ok(p.map(|s| Policy::parse(&s)).unwrap_or_default())
    }

    /// Atomically admit a call by holding `ceiling` tokens as a lease expiring at `now + ttl`, or
    /// deny it if the ceiling would breach a set cap. Reclaims this scope's expired leases first so
    /// a crashed reservation never blocks admission (opportunistic, ADR-0005 D2).
    pub fn reserve_durable(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> rusqlite::Result<ReserveOutcome> {
        let now_ts = now.unix_timestamp();
        let expires_at = (now + ttl).unix_timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Opportunistic reclaim: drop this scope's unsettled, expired leases before measuring.
        tx.execute(
            "DELETE FROM budget_reservation
             WHERE scope = ?1 AND settled = 0 AND expires_at <= ?2",
            params![scope, now_ts],
        )?;

        let budget: Option<(Option<i64>, String, String)> = tx
            .query_row(
                "SELECT limit_tokens, window, policy FROM budget_limit WHERE scope = ?1",
                [scope],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;

        if let Some((Some(limit), window, policy)) = budget {
            // Spend is measured over the scope's current window (calendar-aligned); in-flight
            // reservations (unsettled ceilings) are always counted.
            let start = Self::window_start_ts(now, Window::parse(&window));
            let spent: i64 = tx.query_row(
                "SELECT COALESCE(SUM(actual), 0) FROM budget_reservation
                 WHERE scope = ?1 AND settled = 1 AND COALESCE(settled_at, 0) >= ?2",
                params![scope, start],
                |row| row.get(0),
            )?;
            let reserved = sum_i64(
                &tx,
                "SELECT COALESCE(SUM(ceiling), 0) FROM budget_reservation WHERE scope = ?1 AND settled = 0",
                scope,
            )?;
            // `Warn` is a **soft cap** (ADR-0005 D6): it never denies admission — the lease is still
            // created below so spend keeps accruing for threshold alerts and the dashboard. Only
            // `Block` hard-refuses a ceiling that would breach the cap.
            if Policy::parse(&policy) == Policy::Block && spent + reserved + ceiling as i64 > limit
            {
                // Transaction rolls back on drop — nothing reserved.
                return Ok(ReserveOutcome::Denied(Denied {
                    scope: scope.to_string(),
                    limit: limit.max(0) as u64,
                    spent: spent.max(0) as u64,
                    reserved: reserved.max(0) as u64,
                    requested_ceiling: ceiling,
                }));
            }
        }

        tx.execute(
            "INSERT INTO budget_reservation (scope, ceiling, expires_at) VALUES (?1, ?2, ?3)",
            params![scope, ceiling as i64, expires_at],
        )?;
        let id = tx.last_insert_rowid() as u64;
        tx.commit()?;
        Ok(ReserveOutcome::Admitted(Reservation {
            id,
            scope: scope.to_string(),
            ceiling,
            expires_at: now + ttl,
        }))
    }

    /// Idempotently settle a reservation to its actual billable usage. Guarded by `settled = 0`, so
    /// a retried or replayed settle updates zero rows and changes nothing (ADR-0005 D2/C2).
    pub fn settle_durable(&mut self, reservation_id: u64, actual: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE budget_reservation
             SET actual = ?2, settled = 1, settled_at = CAST(strftime('%s','now') AS INTEGER)
             WHERE id = ?1 AND settled = 0",
            params![reservation_id as i64, actual as i64],
        )?;
        Ok(())
    }

    /// Reclaim every unsettled lease expired at or before `now` (crash/leak backstop); returns how
    /// many were reclaimed. A reclaimed lease releases its held ceiling without recording spend.
    pub fn reclaim_expired_durable(&mut self, now: OffsetDateTime) -> rusqlite::Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM budget_reservation WHERE settled = 0 AND expires_at <= ?1",
            params![now.unix_timestamp()],
        )?;
        Ok(n)
    }

    pub fn limit_durable(&self, scope: &str) -> rusqlite::Result<Option<u64>> {
        let limit: Option<i64> = self
            .conn
            .query_row(
                "SELECT limit_tokens FROM budget_limit WHERE scope = ?1",
                [scope],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(limit.map(|v| v.max(0) as u64))
    }

    /// Settled spend in the scope's **current window** (calendar-aligned). Uses the store as the
    /// clock authority (ADR-0005 D5/D6); a restart re-derives the window from the durable rows.
    pub fn spent_durable(&self, scope: &str) -> rusqlite::Result<u64> {
        let start =
            Self::window_start_ts(OffsetDateTime::now_utc(), self.window_of_durable(scope)?);
        let spent: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(actual), 0) FROM budget_reservation
             WHERE scope = ?1 AND settled = 1 AND COALESCE(settled_at, 0) >= ?2",
            params![scope, start],
            |row| row.get(0),
        )?;
        Ok(spent.max(0) as u64)
    }

    /// Held (unsettled) ceilings. May transiently include not-yet-reclaimed expired leases — a
    /// conservative over-count for a read snapshot; `reserve` reclaims them before admitting.
    pub fn reserved_durable(&self, scope: &str) -> rusqlite::Result<u64> {
        Ok(sum_i64_conn(
            &self.conn,
            "SELECT COALESCE(SUM(ceiling), 0) FROM budget_reservation WHERE scope = ?1 AND settled = 0",
            scope,
        )?
        .max(0) as u64)
    }

    /// Every configured budget (cap + window + policy), for rehydrating the operator's in-memory
    /// budget metadata on startup (ADR-0005 D3). The durable spend/caps already survive a restart;
    /// this recovers the policy/window/limit the dashboard + alert subsystem read.
    pub fn list_budgets_durable(&self) -> rusqlite::Result<Vec<BudgetRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT scope, limit_tokens, window, policy FROM budget_limit ORDER BY scope",
        )?;
        let rows = stmt.query_map([], |row| {
            let scope: String = row.get(0)?;
            let limit: Option<i64> = row.get(1)?;
            let window: String = row.get(2)?;
            let policy: String = row.get(3)?;
            Ok(BudgetRow {
                scope,
                limit: limit.map(|v| v.max(0) as u64),
                window: Window::parse(&window),
                policy: Policy::parse(&policy),
            })
        })?;
        rows.collect()
    }
}

fn sum_i64(tx: &rusqlite::Transaction, sql: &str, scope: &str) -> rusqlite::Result<i64> {
    tx.query_row(sql, [scope], |row| row.get(0))
}

fn sum_i64_conn(conn: &Connection, sql: &str, scope: &str) -> rusqlite::Result<i64> {
    conn.query_row(sql, [scope], |row| row.get(0))
}

impl LedgerView for SqliteLedger {
    fn limit(&self, scope: &str) -> Option<u64> {
        self.limit_durable(scope).ok().flatten()
    }
    fn spent(&self, scope: &str) -> u64 {
        self.spent_durable(scope).unwrap_or(0)
    }
    fn reserved(&self, scope: &str) -> u64 {
        self.reserved_durable(scope).unwrap_or(0)
    }
}

impl EnforcementLedger for SqliteLedger {
    fn set_limit(&mut self, scope: &str, limit: Option<u64>) {
        let _ = self.set_limit_durable(scope, limit, Window::Total, Policy::Block);
    }

    fn reserve(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> Result<Reservation, Denied> {
        // Fail-closed: a backend error denies the call (a hard cap must not admit on a blind write).
        // Per-tier fail-open/closed policy (ADR-0005 D6) belongs at the proxy swap, over the
        // fallible `*_durable` API.
        match self.reserve_durable(scope, ceiling, now, ttl) {
            Ok(ReserveOutcome::Admitted(reservation)) => Ok(reservation),
            Ok(ReserveOutcome::Denied(denied)) => Err(denied),
            Err(_) => Err(Denied {
                scope: scope.to_string(),
                limit: 0,
                spent: 0,
                reserved: 0,
                requested_ceiling: ceiling,
            }),
        }
    }

    fn settle(&mut self, reservation_id: u64, actual: u64) {
        let _ = self.settle_durable(reservation_id, actual);
    }

    fn reclaim_expired(&mut self, now: OffsetDateTime) -> usize {
        self.reclaim_expired_durable(now).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::{Policy, Window};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }
    fn ttl() -> Duration {
        Duration::seconds(60)
    }
    fn mem() -> SqliteLedger {
        SqliteLedger::open(":memory:").unwrap()
    }
    fn admit(l: &mut SqliteLedger, scope: &str, ceiling: u64) -> Reservation {
        match l.reserve_durable(scope, ceiling, t0(), ttl()).unwrap() {
            ReserveOutcome::Admitted(r) => r,
            ReserveOutcome::Denied(d) => panic!("expected admit, denied: {d:?}"),
        }
    }
    fn denied(l: &mut SqliteLedger, scope: &str, ceiling: u64) -> bool {
        matches!(
            l.reserve_durable(scope, ceiling, t0(), ttl()).unwrap(),
            ReserveOutcome::Denied(_)
        )
    }

    #[test]
    fn ceiling_reservation_prevents_overshoot() {
        let mut l = mem();
        l.set_limit_durable("g", Some(100), Window::Total, Policy::Block)
            .unwrap();
        let r = admit(&mut l, "g", 100);
        assert!(
            denied(&mut l, "g", 1),
            "a near-full cap admits nothing more"
        );
        l.settle_durable(r.id, 40).unwrap();
        assert_eq!(l.spent_durable("g").unwrap(), 40);
        assert_eq!(l.reserved_durable("g").unwrap(), 0);
        assert!(l.spent_durable("g").unwrap() + l.reserved_durable("g").unwrap() <= 100);
    }

    #[test]
    fn settle_is_idempotent_under_repeat() {
        let mut l = mem();
        l.set_limit_durable("g", Some(100), Window::Total, Policy::Block)
            .unwrap();
        let r = admit(&mut l, "g", 50);
        l.settle_durable(r.id, 40).unwrap();
        l.settle_durable(r.id, 40).unwrap();
        l.settle_durable(r.id, 999).unwrap(); // a replay must not overwrite or double-count
        assert_eq!(l.spent_durable("g").unwrap(), 40);
        assert_eq!(l.reserved_durable("g").unwrap(), 0);
    }

    #[test]
    fn expired_lease_is_reclaimed_no_capacity_leak() {
        let mut l = mem();
        l.set_limit_durable("g", Some(100), Window::Total, Policy::Block)
            .unwrap();
        let _crashed = admit(&mut l, "g", 80);
        assert_eq!(l.reserved_durable("g").unwrap(), 80);
        // Explicit sweep past the TTL frees it.
        let later = t0() + Duration::seconds(61);
        assert_eq!(l.reclaim_expired_durable(later).unwrap(), 1);
        assert_eq!(l.reserved_durable("g").unwrap(), 0);
        // And a fresh reserve past the TTL reclaims opportunistically (no explicit sweep needed).
        let _crashed2 = l.reserve_durable("g", 80, t0(), ttl()).unwrap();
        match l
            .reserve_durable("g", 80, t0() + Duration::seconds(61), ttl())
            .unwrap()
        {
            ReserveOutcome::Admitted(_) => {}
            ReserveOutcome::Denied(_) => {
                panic!("expired lease should have been reclaimed on reserve")
            }
        }
    }

    #[test]
    fn concurrent_reservations_cannot_oversubscribe() {
        let mut l = mem();
        l.set_limit_durable("g", Some(100), Window::Total, Policy::Block)
            .unwrap();
        let a = admit(&mut l, "g", 60);
        assert!(denied(&mut l, "g", 60), "60 + 60 > 100 must be refused");
        l.settle_durable(a.id, 40).unwrap();
        admit(&mut l, "g", 60); // 40 spent + 60 == 100 fits
        assert!(l.spent_durable("g").unwrap() + l.reserved_durable("g").unwrap() <= 100);
    }

    #[test]
    fn unset_scope_is_unlimited_but_tracked() {
        let mut l = mem();
        let r = admit(&mut l, "free", 1_000_000);
        assert_eq!(l.reserved_durable("free").unwrap(), 1_000_000);
        l.settle_durable(r.id, 999).unwrap();
        assert_eq!(l.spent_durable("free").unwrap(), 999);
    }

    #[test]
    fn spend_survives_reopen() {
        // The property the in-memory ledger lacks (ADR-0005 D3): a restart must not reset spend.
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sandhi_ledger_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let path = path.to_str().unwrap();
        {
            let mut l = SqliteLedger::open(path).unwrap();
            l.set_limit_durable("g", Some(100), Window::Total, Policy::Block)
                .unwrap();
            let r = admit(&mut l, "g", 50);
            l.settle_durable(r.id, 40).unwrap();
        } // connection dropped — simulate a proxy restart
        let reopened = SqliteLedger::open(path).unwrap();
        assert_eq!(
            reopened.spent_durable("g").unwrap(),
            40,
            "spend must persist across restart"
        );
        assert_eq!(
            reopened.limit_durable("g").unwrap(),
            Some(100),
            "limits persist too"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn trait_impl_maps_denied_and_settles() {
        // Exercise the EnforcementLedger trait surface the proxy swap will use.
        let mut l = mem();
        EnforcementLedger::set_limit(&mut l, "g", Some(100));
        let r = EnforcementLedger::reserve(&mut l, "g", 100, t0(), ttl()).expect("fits");
        assert!(EnforcementLedger::reserve(&mut l, "g", 1, t0(), ttl()).is_err());
        EnforcementLedger::settle(&mut l, r.id, 40);
        let view: &dyn LedgerView = &l;
        assert_eq!(view.spent("g"), 40);
        assert_eq!(view.available("g"), 60);
    }
    #[test]
    fn warn_policy_admits_over_cap_but_still_tracks_spend() {
        // ADR-0005 D6: a `Warn` scope is a soft cap — reserve never denies (the lease is created so
        // spend accrues for alerts), even past the configured limit. A `Block` scope with the same
        // numbers would refuse.
        let mut l = mem();
        l.set_limit_durable("w", Some(100), Window::Total, Policy::Warn)
            .unwrap();
        let a = admit(&mut l, "w", 80);
        l.settle_durable(a.id, 80).unwrap();
        // 80 spent, cap 100 — a 60-ceiling call would breach it, but Warn admits anyway.
        let b = match l.reserve_durable("w", 60, t0(), ttl()).unwrap() {
            ReserveOutcome::Admitted(r) => r,
            ReserveOutcome::Denied(d) => panic!("warn must not deny over cap: {d:?}"),
        };
        l.settle_durable(b.id, 60).unwrap();
        assert_eq!(
            l.spent_durable("w").unwrap(),
            140,
            "spend accrues past a warn cap"
        );

        // The same shape under Block hard-denies.
        l.set_limit_durable("b", Some(100), Window::Total, Policy::Block)
            .unwrap();
        let c = admit(&mut l, "b", 80);
        l.settle_durable(c.id, 80).unwrap();
        assert!(
            denied(&mut l, "b", 60),
            "block refuses a ceiling over the cap"
        );
    }

    #[test]
    fn list_budgets_durable_round_trips_for_rehydration() {
        let mut l = mem();
        l.set_limit_durable("a", Some(1000), Window::Daily, Policy::Block)
            .unwrap();
        l.set_limit_durable("b", Some(500), Window::Total, Policy::Warn)
            .unwrap();
        let rows = l.list_budgets_durable().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            BudgetRow {
                scope: "a".into(),
                limit: Some(1000),
                window: Window::Daily,
                policy: Policy::Block,
            }
        );
        assert_eq!(
            rows[1],
            BudgetRow {
                scope: "b".into(),
                limit: Some(500),
                window: Window::Total,
                policy: Policy::Warn,
            }
        );
    }

    #[test]
    fn daily_window_excludes_prior_window_spend() {
        // Spend is measured over the current calendar window; an old settled reservation (before
        // today's UTC midnight) does not count against a `daily` cap. Survives restart because the
        // window is re-derived from durable rows (ADR-0005 D3/D6).
        let mut l = mem();
        l.set_limit_durable("d", Some(1000), Window::Daily, Policy::Block)
            .unwrap();
        assert_eq!(l.window_of_durable("d").unwrap(), Window::Daily);
        assert_eq!(l.policy_of_durable("d").unwrap(), Policy::Block);

        // A settled reservation stamped two days ago (outside today's window).
        l.conn
            .execute(
                "INSERT INTO budget_reservation (scope, ceiling, actual, settled, settled_at, expires_at)
                 VALUES ('d', 800, 800, 1, CAST(strftime('%s','now','-2 days') AS INTEGER), 0)",
                [],
            )
            .unwrap();
        // Today's spend is 0 → a fresh 900-token call fits under the 1000 daily cap.
        assert_eq!(l.spent_durable("d").unwrap(), 0);
        let r = match l
            .reserve_durable("d", 900, OffsetDateTime::now_utc(), ttl())
            .unwrap()
        {
            ReserveOutcome::Admitted(r) => r,
            ReserveOutcome::Denied(dn) => panic!("prior-window spend must not block today: {dn:?}"),
        };
        l.settle_durable(r.id, 900).unwrap();
        assert_eq!(l.spent_durable("d").unwrap(), 900);
        // Now the daily window is nearly full: a second 200 would exceed 1000.
        assert!(matches!(
            l.reserve_durable("d", 200, OffsetDateTime::now_utc(), ttl())
                .unwrap(),
            ReserveOutcome::Denied(_)
        ));
    }
}
