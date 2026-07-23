//! Lease-based enforcement ledger (ADR-0005 D1/D2/D5).
//!
//! The correctness core the proxy budget gate will move onto. Unlike [`crate::budget::BudgetLedger`]
//! — a delta counter that reconciles to the *actual* measured usage, i.e. a **soft** cap that a
//! single streaming request can overshoot (ADR-0005 Context) — this ledger:
//!
//! - **reserves a ceiling, not an estimate** ([`EnforcementLedger::reserve`]): a conservative upper
//!   bound is held so admitting a call can never overshoot a hard cap. The proxy's mid-stream cutoff
//!   guarantees actual ≤ ceiling; this ledger guarantees `spent + reserved ≤ limit` at all times.
//! - **holds it as a TTL lease** ([`Reservation`]): a lease left dangling by a crash is reclaimed
//!   ([`EnforcementLedger::reclaim_expired`]) rather than leaking capacity forever.
//! - **settles idempotently by id** ([`EnforcementLedger::settle`]): an at-least-once settle (retry,
//!   failover replay) is a no-op on repeat — no double counting.
//!
//! In-memory here (the SDK/test substrate); the durable, atomic, *shared* impls (SQLite single-node,
//! Redis HA) implement the same traits in `sandhi-store` with the atomicity/TTL/calendar-window math
//! behind the trait (ADR-0005 D5/D6). Enforcement is in neutral **tokens** — no dollars.

use std::collections::HashMap;

use time::{Duration, OffsetDateTime};

/// A held reservation — a lease over `ceiling` tokens in `scope`, valid until `expires_at`.
///
/// The `id` is opaque and assigned by the ledger; the caller settles or lets it expire by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub id: u64,
    pub scope: String,
    pub ceiling: u64,
    pub expires_at: OffsetDateTime,
}

/// Why a [`reserve`](EnforcementLedger::reserve) was refused: the ceiling did not fit under the cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub scope: String,
    pub limit: u64,
    pub spent: u64,
    pub reserved: u64,
    pub requested_ceiling: u64,
}

/// Read snapshot of a scope's accounting — the surface the pure policy engine reads (ADR-0005 D5).
///
/// Object-safe on purpose: a `&dyn LedgerView` snapshot is what the decision function consumes.
pub trait LedgerView {
    /// The configured cap for a scope. `None` = no cap set (unlimited but still tracked).
    fn limit(&self, scope: &str) -> Option<u64>;
    /// Settled billable tokens in the current window.
    fn spent(&self, scope: &str) -> u64;
    /// Tokens held by in-flight leases.
    fn reserved(&self, scope: &str) -> u64;
    /// Remaining headroom = `limit - spent - reserved` (saturating). Unlimited scopes report
    /// [`u64::MAX`].
    fn available(&self, scope: &str) -> u64 {
        match self.limit(scope) {
            None => u64::MAX,
            Some(limit) => {
                limit.saturating_sub(self.spent(scope).saturating_add(self.reserved(scope)))
            }
        }
    }
}

/// The write side: atomic ceiling-reserve, idempotent settle, and lease reclaim (ADR-0005 D2).
///
/// The three operations are the invariant that makes a durable/shared impl correct; the in-memory
/// [`InMemoryLedger`] is the reference implementation of the contract.
pub trait EnforcementLedger: LedgerView {
    /// Set (or clear, with `None`) the cap for a scope. Enforcement only — no pricing.
    fn set_limit(&mut self, scope: &str, limit: Option<u64>);

    /// Atomically admit a call by holding `ceiling` tokens as a lease expiring at `now + ttl`.
    ///
    /// The check is `spent + reserved + ceiling ≤ limit`; a scope with no cap always admits.
    /// Returns [`Denied`] when the ceiling would breach a set cap — the call is never dispatched,
    /// so a hard cap cannot be overshot even before any usage is known.
    fn reserve(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> Result<Reservation, Denied>;

    /// Idempotently settle a reservation to its actual billable usage, releasing the lease.
    ///
    /// A no-op if `reservation_id` is unknown (already settled, or reclaimed after expiry) — safe
    /// under at-least-once delivery. `actual` is the neutral billable quantity the caller computed
    /// (e.g. via `billable()`); it is trusted to be ≤ the reserved ceiling (the proxy's mid-stream
    /// cutoff enforces that bound).
    fn settle(&mut self, reservation_id: u64, actual: u64);

    /// Reclaim every lease that expired at or before `now` (crash/leak backstop) and return how
    /// many were reclaimed. A reclaimed lease releases its held ceiling **without** recording spend
    /// — so `ttl` must exceed the longest legitimate call, and the proxy should settle
    /// (incl. `Partial` on disconnect) before the TTL elapses.
    fn reclaim_expired(&mut self, now: OffsetDateTime) -> usize;
}

/// In-memory reference ledger (SDK / tests). Correct under single-process concurrency; the durable
/// and shared variants live in `sandhi-store` behind the same traits (ADR-0005 D5).
#[derive(Debug, Default)]
pub struct InMemoryLedger {
    limits: HashMap<String, Option<u64>>,
    spent: HashMap<String, u64>,
    reserved: HashMap<String, u64>,
    leases: HashMap<u64, Reservation>,
    next_id: u64,
}

impl InMemoryLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently-held (unsettled, unexpired) leases — for tests/introspection.
    #[must_use]
    pub fn active_leases(&self) -> usize {
        self.leases.len()
    }

    fn add_reserved(&mut self, scope: &str, delta: u64) {
        *self.reserved.entry(scope.to_string()).or_insert(0) += delta;
    }

    fn sub_reserved(&mut self, scope: &str, delta: u64) {
        let held = self.reserved.entry(scope.to_string()).or_insert(0);
        *held = held.saturating_sub(delta);
    }
}

impl LedgerView for InMemoryLedger {
    fn limit(&self, scope: &str) -> Option<u64> {
        self.limits.get(scope).copied().flatten()
    }

    fn spent(&self, scope: &str) -> u64 {
        self.spent.get(scope).copied().unwrap_or(0)
    }

    fn reserved(&self, scope: &str) -> u64 {
        self.reserved.get(scope).copied().unwrap_or(0)
    }
}

impl EnforcementLedger for InMemoryLedger {
    fn set_limit(&mut self, scope: &str, limit: Option<u64>) {
        self.limits.insert(scope.to_string(), limit);
    }

    fn reserve(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> Result<Reservation, Denied> {
        if let Some(limit) = self.limit(scope) {
            let spent = self.spent(scope);
            let reserved = self.reserved(scope);
            if spent.saturating_add(reserved).saturating_add(ceiling) > limit {
                return Err(Denied {
                    scope: scope.to_string(),
                    limit,
                    spent,
                    reserved,
                    requested_ceiling: ceiling,
                });
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let reservation = Reservation {
            id,
            scope: scope.to_string(),
            ceiling,
            expires_at: now + ttl,
        };
        self.leases.insert(id, reservation.clone());
        self.add_reserved(scope, ceiling);
        Ok(reservation)
    }

    fn settle(&mut self, reservation_id: u64, actual: u64) {
        let Some(lease) = self.leases.remove(&reservation_id) else {
            return; // unknown / already settled / reclaimed — idempotent no-op.
        };
        self.sub_reserved(&lease.scope, lease.ceiling);
        *self.spent.entry(lease.scope).or_insert(0) += actual;
    }

    fn reclaim_expired(&mut self, now: OffsetDateTime) -> usize {
        let expired: Vec<u64> = self
            .leases
            .iter()
            .filter(|(_, l)| l.expires_at <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            if let Some(lease) = self.leases.remove(id) {
                self.sub_reserved(&lease.scope, lease.ceiling);
            }
        }
        expired.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn ttl() -> Duration {
        Duration::seconds(60)
    }

    #[test]
    fn ceiling_reservation_prevents_overshoot() {
        // The 100x-overshoot bug (ADR-0005 Context #1) cannot happen: the ceiling is held, so a
        // near-exhausted cap refuses a call whose worst case would breach it — before any usage.
        let mut l = InMemoryLedger::new();
        l.set_limit("g", Some(100));
        // A call that could emit up to 100 tokens fits when empty...
        let r = l.reserve("g", 100, t0(), ttl()).expect("fits");
        // ...and now nothing else does, even 1 token.
        let denied = l.reserve("g", 1, t0(), ttl()).unwrap_err();
        assert_eq!(denied.reserved, 100);
        assert_eq!(denied.limit, 100);
        // Real usage came in far under the ceiling; settle frees the difference.
        l.settle(r.id, 40);
        assert_eq!(l.spent("g"), 40);
        assert_eq!(l.reserved("g"), 0);
        assert_eq!(l.available("g"), 60);
        // Invariant holds throughout: spent + reserved never exceeds the limit.
        assert!(l.spent("g") + l.reserved("g") <= 100);
    }

    #[test]
    fn settle_is_idempotent_under_repeat() {
        let mut l = InMemoryLedger::new();
        l.set_limit("g", Some(100));
        let r = l.reserve("g", 50, t0(), ttl()).unwrap();
        l.settle(r.id, 40);
        l.settle(r.id, 40); // at-least-once repeat
        l.settle(r.id, 40);
        assert_eq!(l.spent("g"), 40, "repeat settle must not double-count");
        assert_eq!(l.reserved("g"), 0);
    }

    #[test]
    fn expired_lease_is_reclaimed_no_capacity_leak() {
        // A crash between reserve and settle leaves a dangling lease; without reclaim it would block
        // the scope forever (ADR-0005 Context #3a). Reclaim frees it after the TTL.
        let mut l = InMemoryLedger::new();
        l.set_limit("g", Some(100));
        let _r = l.reserve("g", 80, t0(), ttl()).unwrap();
        assert_eq!(l.available("g"), 20);
        // Not yet expired.
        assert_eq!(l.reclaim_expired(t0()), 0);
        assert_eq!(l.available("g"), 20);
        // Past the TTL → reclaimed, capacity restored.
        let later = t0() + Duration::seconds(61);
        assert_eq!(l.reclaim_expired(later), 1);
        assert_eq!(l.reserved("g"), 0);
        assert_eq!(l.available("g"), 100);
        assert_eq!(l.active_leases(), 0);
    }

    #[test]
    fn concurrent_reservations_cannot_oversubscribe() {
        let mut l = InMemoryLedger::new();
        l.set_limit("g", Some(100));
        let a = l.reserve("g", 60, t0(), ttl()).unwrap();
        // Second in-flight reservation cannot fit (60 + 60 > 100).
        assert!(l.reserve("g", 60, t0(), ttl()).is_err());
        // First settles cheaper; now a 60 ceiling fits (40 + 60 == 100).
        l.settle(a.id, 40);
        assert!(l.reserve("g", 60, t0(), ttl()).is_ok());
        assert!(l.spent("g") + l.reserved("g") <= 100);
    }

    #[test]
    fn unset_scope_is_unlimited_but_tracked() {
        let mut l = InMemoryLedger::new();
        assert_eq!(l.available("free"), u64::MAX);
        let r = l.reserve("free", 1_000_000, t0(), ttl()).unwrap();
        assert_eq!(l.reserved("free"), 1_000_000);
        l.settle(r.id, 999);
        assert_eq!(l.spent("free"), 999);
    }

    #[test]
    fn settle_after_reclaim_is_a_noop_and_loses_the_usage() {
        // Documents the TTL tradeoff: a settle arriving after reclaim is dropped. TTL must exceed
        // the longest legitimate call; the proxy settles (incl. Partial-on-disconnect) before TTL.
        let mut l = InMemoryLedger::new();
        l.set_limit("g", Some(100));
        let r = l.reserve("g", 50, t0(), ttl()).unwrap();
        assert_eq!(l.reclaim_expired(t0() + Duration::seconds(61)), 1);
        l.settle(r.id, 40); // too late
        assert_eq!(l.spent("g"), 0);
        assert_eq!(l.reserved("g"), 0);
    }

    #[test]
    fn ledger_view_snapshot_reflects_state() {
        let mut l = InMemoryLedger::new();
        l.set_limit("g", Some(100));
        let _ = l.reserve("g", 30, t0(), ttl()).unwrap();
        let view: &dyn LedgerView = &l;
        assert_eq!(view.limit("g"), Some(100));
        assert_eq!(view.reserved("g"), 30);
        assert_eq!(view.spent("g"), 0);
        assert_eq!(view.available("g"), 70);
    }
}
