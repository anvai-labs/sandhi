//! The proxy's enforcement ledger — the ADR-0005 lease model, made durable when `SANDHI_STORE` is
//! set (ADR-0005 step 2, the proxy repoint).
//!
//! [`ProxyState.ledger`](crate::ProxyState) is a [`ProxyLedger`]: one of two arms behind the same
//! reserve-a-ceiling → settle-by-id contract.
//!
//! - [`ProxyLedger::Durable`] — a [`SqliteLedger`]: caps + spend + in-flight leases persist, so a
//!   restart no longer zeroes accrued spend (ADR-0005 D3), spend is measured over calendar windows
//!   (D5), and a crashed request's lease is reclaimed rather than leaking capacity (D2).
//! - [`ProxyLedger::Memory`] — a [`InMemoryLedger`]: the volatile dev / fallback path (spend + leases
//!   reset on restart). It carries no policy notion of its own, so a `Warn` (soft-cap) scope is
//!   stored uncapped here; the durable arm enforces `Warn` natively.
//!
//! D6 fail-open/closed lives here, over the durable arm's fallible API: a `Block` scope fails closed
//! (a backend error denies — a hard cap must never admit on a blind write), a `Warn` scope fails
//! open (admit, unmetered). Neutral tokens throughout — no dollars.

use time::{Duration, OffsetDateTime};

use sandhi_core::{EnforcementLedger, InMemoryLedger, LedgerView, Policy, Reservation, Window};
use sandhi_store::{ReserveOutcome, SqliteLedger};

/// Lease TTL. Must exceed the longest legitimate call (a slow stream can run minutes) so a lease is
/// only reclaimed well after the request could still be settling (ADR-0005 D2). The proxy settles
/// every request — including `Partial`-on-disconnect — via the `Drop` finalizer long before this.
const RESERVATION_TTL_SECS: i64 = 900; // 15 minutes

/// Outcome of admitting one call against the ledger (ADR-0005 D1/D6).
pub enum Admission {
    /// Admitted with a lease to settle by id after the call finalizes.
    Leased(Reservation),
    /// Admitted **without** a durable lease — a `Warn` scope failing open on a backend error (D6).
    /// Settle is skipped (there is nothing to settle); the usage event still emits.
    Unmetered,
    /// Refused: the ceiling would breach a hard (`Block`) cap.
    Denied,
}

/// The proxy's budget ledger. Both arms use the ADR-0005 lease model; the durable arm additionally
/// carries crash-safe leases + calendar windows.
pub enum ProxyLedger {
    Memory(InMemoryLedger),
    Durable(SqliteLedger),
}

impl ProxyLedger {
    /// A volatile in-memory ledger (dev / no-`SANDHI_STORE` fallback).
    #[must_use]
    pub fn in_memory() -> Self {
        Self::Memory(InMemoryLedger::new())
    }

    /// Open a durable SQLite ledger at `path` (may share the usage-store file — its tables are
    /// disjoint). Returns the backend error as a string so callers need not depend on `rusqlite`.
    pub fn durable(path: &str) -> Result<Self, String> {
        SqliteLedger::open(path)
            .map(Self::Durable)
            .map_err(|e| e.to_string())
    }

    fn ttl() -> Duration {
        Duration::seconds(RESERVATION_TTL_SECS)
    }

    /// Set (or clear, with `limit = None`) a scope's budget: cap + window + policy.
    ///
    /// The durable arm stores the real limit + policy, so it enforces `Block` vs. `Warn` natively
    /// and the metadata survives a restart. The in-memory arm has no policy notion, so a `Warn`
    /// scope is stored **uncapped** (it must never deny) — its configured limit lives only in the
    /// operator's in-memory budgets map (volatile, which is fine for the volatile arm).
    pub fn set_budget(&mut self, scope: &str, limit: Option<u64>, window: Window, policy: Policy) {
        match self {
            Self::Memory(l) => {
                let enforced = match policy {
                    Policy::Block => limit,
                    Policy::Warn => None,
                };
                l.set_limit(scope, enforced);
            }
            Self::Durable(l) => {
                if let Err(e) = l.set_limit_durable(scope, limit, window, policy) {
                    eprintln!("sandhi-proxy: durable set_budget failed for {scope}: {e}");
                }
            }
        }
    }

    /// Admit a call by reserving `ceiling` tokens as a lease valid for [`RESERVATION_TTL_SECS`].
    /// Applies ADR-0005 D6 on a durable backend error: `Warn` fails open ([`Admission::Unmetered`]),
    /// `Block` fails closed ([`Admission::Denied`]). An over-cap ceiling is [`Admission::Denied`]
    /// regardless (the durable arm never denies a `Warn` scope — it is a soft cap).
    pub fn reserve(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        policy: Policy,
    ) -> Admission {
        match self {
            Self::Memory(l) => match l.reserve(scope, ceiling, now, Self::ttl()) {
                Ok(r) => Admission::Leased(r),
                Err(_) => Admission::Denied,
            },
            Self::Durable(l) => match l.reserve_durable(scope, ceiling, now, Self::ttl()) {
                Ok(ReserveOutcome::Admitted(r)) => Admission::Leased(r),
                Ok(ReserveOutcome::Denied(_)) => Admission::Denied,
                Err(e) => {
                    eprintln!("sandhi-proxy: durable reserve failed for {scope}: {e}");
                    match policy {
                        Policy::Warn => Admission::Unmetered,
                        Policy::Block => Admission::Denied,
                    }
                }
            },
        }
    }

    /// Idempotently settle a lease to its actual billable usage (`actual = 0` releases it without
    /// recording spend — the failed/cancelled case). A no-op if the id is unknown (already settled
    /// or reclaimed), so the `Drop` finalizer firing after an explicit finalize is safe.
    pub fn settle(&mut self, reservation: &Reservation, actual: u64) {
        match self {
            Self::Memory(l) => l.settle(reservation.id, actual),
            Self::Durable(l) => {
                if let Err(e) = l.settle_durable(reservation.id, actual) {
                    eprintln!(
                        "sandhi-proxy: durable settle failed for reservation {}: {e}",
                        reservation.id
                    );
                }
            }
        }
    }

    /// The configured cap for a scope (`None` when unset/unlimited). The in-memory arm reports
    /// `None` for a `Warn` scope (stored uncapped); the durable arm reports the real limit.
    pub fn limit(&self, scope: &str) -> Option<u64> {
        match self {
            Self::Memory(l) => l.limit(scope),
            Self::Durable(l) => l.limit_durable(scope).unwrap_or(None),
        }
    }

    /// Settled billable spend in the scope's current window.
    pub fn spent(&self, scope: &str) -> u64 {
        match self {
            Self::Memory(l) => l.spent(scope),
            Self::Durable(l) => l.spent_durable(scope).unwrap_or(0),
        }
    }

    /// Tokens held by in-flight leases.
    pub fn reserved(&self, scope: &str) -> u64 {
        match self {
            Self::Memory(l) => l.reserved(scope),
            Self::Durable(l) => l.reserved_durable(scope).unwrap_or(0),
        }
    }

    /// Every configured budget (cap + window + policy). The durable arm reads them back from SQLite
    /// for restart rehydration; the volatile in-memory arm has nothing to persist, so it returns an
    /// empty list (its budget metadata lives only in the operator's in-memory map).
    pub fn budgets(&self) -> Vec<sandhi_store::BudgetRow> {
        match self {
            Self::Memory(_) => Vec::new(),
            Self::Durable(l) => l.list_budgets_durable().unwrap_or_default(),
        }
    }

    /// Reclaim every lease expired at or before `now` (crash/leak backstop); returns the count.
    pub fn reclaim_expired(&mut self, now: OffsetDateTime) -> usize {
        match self {
            Self::Memory(l) => l.reclaim_expired(now),
            Self::Durable(l) => l.reclaim_expired_durable(now).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    #[test]
    fn block_scope_denies_over_cap_and_settles_by_lease() {
        let mut l = ProxyLedger::in_memory();
        l.set_budget("g", Some(100), Window::Total, Policy::Block);
        let Admission::Leased(r) = l.reserve("g", 100, now(), Policy::Block) else {
            panic!("first fits");
        };
        assert!(matches!(
            l.reserve("g", 1, now(), Policy::Block),
            Admission::Denied
        ));
        l.settle(&r, 40);
        assert_eq!(l.spent("g"), 40);
        assert_eq!(l.reserved("g"), 0);
    }

    #[test]
    fn warn_scope_admits_over_cap_in_memory_arm() {
        // The in-memory arm stores a Warn scope uncapped, so it never denies (matches the durable
        // arm's soft-cap behavior). Spend still tracks.
        let mut l = ProxyLedger::in_memory();
        l.set_budget("g", Some(100), Window::Total, Policy::Warn);
        assert!(l.limit("g").is_none(), "warn is stored uncapped in-memory");
        let Admission::Leased(a) = l.reserve("g", 80, now(), Policy::Warn) else {
            panic!("admits");
        };
        l.settle(&a, 80);
        let Admission::Leased(b) = l.reserve("g", 80, now(), Policy::Warn) else {
            panic!("warn admits past the cap");
        };
        l.settle(&b, 80);
        assert_eq!(l.spent("g"), 160);
    }

    #[test]
    fn zero_settle_releases_without_recording_spend() {
        let mut l = ProxyLedger::in_memory();
        l.set_budget("g", Some(100), Window::Total, Policy::Block);
        let Admission::Leased(r) = l.reserve("g", 50, now(), Policy::Block) else {
            panic!("fits");
        };
        assert_eq!(l.reserved("g"), 50);
        l.settle(&r, 0); // the cancelled/failed path
        assert_eq!(l.reserved("g"), 0);
        assert_eq!(l.spent("g"), 0);
    }

    #[test]
    fn durable_arm_persists_spend_and_budgets_across_reopen() {
        // The whole point of the swap (ADR-0005 D3): a restart must NOT zero accrued spend or the
        // operator's budget metadata. Also drives `rehydrate_budgets` recovering the [`BudgetSpec`]
        // map a fresh process starts with empty.
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Mutex;

        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sandhi_proxy_ledger_{}_{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let path = path.to_str().unwrap();

        {
            let mut l = ProxyLedger::durable(path).unwrap();
            l.set_budget("g", Some(1000), Window::Daily, Policy::Block);
            let Admission::Leased(r) = l.reserve("g", 100, now(), Policy::Block) else {
                panic!("fits under the 1000 cap");
            };
            l.settle(&r, 80);
            assert_eq!(l.spent("g"), 80);
            assert_eq!(l.reserved("g"), 0);
        } // connection dropped — simulate a proxy restart

        let reopened = ProxyLedger::durable(path).unwrap();
        assert_eq!(
            reopened.spent("g"),
            80,
            "durable spend survives restart (the property the in-memory ledger lacks)"
        );
        assert_eq!(reopened.limit("g"), Some(1000), "the cap persists too");

        // Rehydrate the operator budget metadata from the durable rows into a fresh (empty) map.
        let budgets: Mutex<std::collections::HashMap<String, crate::BudgetSpec>> =
            Mutex::new(std::collections::HashMap::new());
        crate::rehydrate_budgets(&reopened, &budgets);
        let map = budgets.lock().unwrap();
        let spec = map.get("g").expect("budget rehydrated");
        assert_eq!(spec.limit_tokens, 1000);
        assert_eq!(spec.window, "daily");
        assert_eq!(spec.policy, "block");

        let _ = std::fs::remove_file(path);
    }
}
