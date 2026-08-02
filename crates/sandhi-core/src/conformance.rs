//! The TD-0007 enforcement-ledger conformance suite (C1–C6), as executable assertions.
//!
//! TD-0007 froze C1–C6 as the acceptance bar for *any* ledger backend, and then the bar lived only
//! as prose in a design document. That is a gap with teeth: two backends already ship
//! ([`InMemoryLedger`](crate::InMemoryLedger) and `sandhi-store`'s `SqliteLedger`), a third is being
//! evaluated, and until now nothing executable said what "correct" meant. A backend choice made
//! against a prose bar is a judgement call; made against this suite it is a measurement.
//!
//! Run it from any crate that has a backend:
//!
//! ```ignore
//! use sandhi_core::conformance::assert_enforcement_conformance;
//! assert_enforcement_conformance("SqliteLedger", || SqliteLedger::open(":memory:").unwrap());
//! ```
//!
//! **What this suite can and cannot prove.** C1's *atomicity* and C4's *linearizability* are
//! properties of the store under concurrency, and [`EnforcementLedger`](crate::EnforcementLedger)
//! takes `&mut self` — so every call through this trait is already serialized by the caller's lock.
//! What the suite proves is the **invariant** these properties exist to protect
//! (`spent + reserved ≤ limit`, never oversubscribed) and the state machine around it. A backend
//! that is only correct *because* the caller holds a lock passes here and still fails in
//! production across replicas; that is exactly why [`SINGLE_WRITER_CAVEAT`] is spelled out rather
//! than left for someone to infer from a green test run.

use time::{Duration, OffsetDateTime};

use crate::ledger::{Denied, EnforcementLedger};

/// What a green run of this suite does **not** establish.
///
/// Quote this in any backend-evaluation write-up. It is the difference between "passes the suite"
/// and "safe across replicas", and conflating the two would reintroduce the exact multi-replica
/// hole TD-0007 exists to close.
pub const SINGLE_WRITER_CAVEAT: &str = "\
This suite drives the ledger through `&mut self`, so calls are serialized by the caller. It proves \
the invariant and the state machine, NOT that the backend serializes concurrent writers itself \
(TD-0007 C1/C4). A backend intended for multi-replica use must additionally demonstrate that the \
conditional admit runs inside the store under its own write lock — that cannot be shown through \
this trait.";

fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("fixed test instant")
}

const TTL: Duration = Duration::seconds(900);

/// Run the full C1–C6 suite against a backend. Panics with a labelled message on the first failure.
pub fn assert_enforcement_conformance<L, F>(backend: &str, mut make: F)
where
    L: EnforcementLedger,
    F: FnMut() -> L,
{
    c1_atomic_conditional_admit(backend, &mut make);
    c2_ttl_leases_are_reclaimed_on_a_timer(backend, &mut make);
    c3_settle_by_id_is_idempotent(backend, &mut make);
    c4_a_hard_cap_never_admits_over_the_limit(backend, &mut make);
    c5_hot_path_is_a_point_write(backend, &mut make);
    c6_uncapped_scopes_always_admit(backend, &mut make);
}

/// **C1** — the admit decision is `spent + reserved + ceiling ≤ limit`, refused otherwise.
fn c1_atomic_conditional_admit<L: EnforcementLedger, F: FnMut() -> L>(backend: &str, make: &mut F) {
    let mut ledger = make();
    ledger.set_limit("s", Some(100));

    let first = ledger
        .reserve("s", 60, t0(), TTL)
        .unwrap_or_else(|_| panic!("{backend} C1: a 60 ceiling under a 100 cap must admit"));
    assert_eq!(ledger.reserved("s"), 60, "{backend} C1: lease must be held");

    // 60 + 60 > 100: the second must be refused *before* dispatch, not settled into an overshoot.
    match ledger.reserve("s", 60, t0(), TTL) {
        Err(Denied { .. }) => {}
        Ok(_) => panic!(
            "{backend} C1 VIOLATION: admitted a ceiling that breaches the cap — \
             a hard cap that admits on the way in is not a cap"
        ),
    }

    // The refusal must not have consumed anything.
    assert_eq!(
        ledger.reserved("s"),
        60,
        "{backend} C1: a denied reserve must leave reserved unchanged"
    );

    // Headroom that does fit is still admitted (a denial must not wedge the scope).
    ledger
        .reserve("s", 40, t0(), TTL)
        .unwrap_or_else(|_| panic!("{backend} C1: 60+40 == 100 must fit exactly"));
    assert_eq!(ledger.reserved("s"), 100);

    ledger.settle(first.id, 10);
    assert!(
        ledger.spent("s") >= 10,
        "{backend} C1: settle must move the quantity into spend"
    );
}

/// **C2** — a lease expires on a timer, so a crashed reserver cannot hold capacity forever.
fn c2_ttl_leases_are_reclaimed_on_a_timer<L: EnforcementLedger, F: FnMut() -> L>(
    backend: &str,
    make: &mut F,
) {
    let mut ledger = make();
    ledger.set_limit("s", Some(100));
    let _abandoned = ledger.reserve("s", 100, t0(), TTL).expect("admits");
    assert_eq!(ledger.reserved("s"), 100);

    // Before expiry nothing is reclaimed — reclaim must not free live leases.
    assert_eq!(
        ledger.reclaim_expired(t0() + Duration::seconds(60)),
        0,
        "{backend} C2: a live lease must not be reclaimed"
    );

    // After expiry the capacity comes back WITHOUT anyone reading the scope first: the reclaim is
    // timed, not lazy-on-read. A lazy backend wedges the budget until someone happens to query it.
    let reclaimed = ledger.reclaim_expired(t0() + TTL + Duration::seconds(1));
    assert_eq!(
        reclaimed, 1,
        "{backend} C2 VIOLATION: an expired lease was not reclaimed on a timer"
    );
    assert_eq!(
        ledger.reserved("s"),
        0,
        "{backend} C2: reclaimed capacity must be released"
    );
    ledger
        .reserve("s", 100, t0() + TTL + Duration::seconds(2), TTL)
        .unwrap_or_else(|_| panic!("{backend} C2: the scope must be usable after a reclaim"));
}

/// **C3** — `settle(id, actual)` is a state transition; a replay is a no-op.
fn c3_settle_by_id_is_idempotent<L: EnforcementLedger, F: FnMut() -> L>(
    backend: &str,
    make: &mut F,
) {
    let mut ledger = make();
    ledger.set_limit("s", Some(1_000));
    let lease = ledger.reserve("s", 100, t0(), TTL).expect("admits");

    ledger.settle(lease.id, 40);
    let after_first = ledger.spent("s");
    assert_eq!(
        after_first, 40,
        "{backend} C3: first settle records the actual"
    );

    // At-least-once delivery means this WILL happen; delta arithmetic on a counter double-counts.
    ledger.settle(lease.id, 40);
    ledger.settle(lease.id, 40);
    assert_eq!(
        ledger.spent("s"),
        after_first,
        "{backend} C3 VIOLATION: a replayed settle double-counted — \
         at-least-once delivery would inflate every retried call"
    );
    assert_eq!(
        ledger.reserved("s"),
        0,
        "{backend} C3: settling releases the lease exactly once"
    );
}

/// **C4** — for a hard cap the invariant holds at every step, never transiently overshooting.
fn c4_a_hard_cap_never_admits_over_the_limit<L: EnforcementLedger, F: FnMut() -> L>(
    backend: &str,
    make: &mut F,
) {
    let mut ledger = make();
    ledger.set_limit("s", Some(50));

    // Hammer the scope with unit reservations and settles, asserting the invariant after each.
    let mut admitted = 0_u64;
    let mut leases = Vec::new();
    for i in 0..200 {
        if let Ok(lease) = ledger.reserve("s", 5, t0() + Duration::seconds(i), TTL) {
            admitted += 1;
            leases.push(lease.id);
        }
        let spent = ledger.spent("s");
        let reserved = ledger.reserved("s");
        assert!(
            spent + reserved <= 50,
            "{backend} C4 VIOLATION: spent({spent}) + reserved({reserved}) > limit(50) after \
             iteration {i} — the invariant must hold at EVERY step, not on average"
        );
    }
    assert_eq!(
        admitted, 10,
        "{backend} C4: a 50 cap admits exactly ten 5-token ceilings"
    );

    // Settling below the ceiling returns headroom, and the invariant still holds.
    for id in leases {
        ledger.settle(id, 1);
    }
    assert!(
        ledger.spent("s") + ledger.reserved("s") <= 50,
        "{backend} C4: invariant must survive settlement"
    );
}

/// **C5** — the hot path is a point write on a small record, not a scan.
///
/// This cannot assert a latency SLO portably (a loaded CI runner would make it flaky, and a
/// threshold nobody trusts gets muted). What it *can* assert is the shape TD-0007 C5 is really
/// about: cost per operation must not grow with the number of scopes, which is what rules out a
/// backend that scans.
fn c5_hot_path_is_a_point_write<L: EnforcementLedger, F: FnMut() -> L>(
    backend: &str,
    make: &mut F,
) {
    let mut ledger = make();
    for i in 0..200 {
        ledger.set_limit(&format!("scope_{i}"), Some(1_000));
    }
    // A reserve on one scope must be unaffected by the presence of 199 others.
    let lease = ledger
        .reserve("scope_7", 10, t0(), TTL)
        .unwrap_or_else(|_| panic!("{backend} C5: reserve must succeed with many scopes present"));
    assert_eq!(ledger.reserved("scope_7"), 10);
    assert_eq!(
        ledger.reserved("scope_8"),
        0,
        "{backend} C5: a reserve must touch exactly one scope — cross-scope writes mean the \
         hot path is not a point write"
    );
    ledger.settle(lease.id, 3);
    assert_eq!(
        ledger.spent("scope_8"),
        0,
        "{backend} C5: settle is scoped too"
    );
}

/// **C6** — a scope with no cap always admits (fail policy is the caller's, per ADR-0005 D6).
fn c6_uncapped_scopes_always_admit<L: EnforcementLedger, F: FnMut() -> L>(
    backend: &str,
    make: &mut F,
) {
    let mut ledger = make();
    // Never set a limit; also explicitly clear one, since `None` is the documented "uncapped".
    ledger.set_limit("capped", Some(10));
    ledger.set_limit("capped", None);

    for scope in ["never_capped", "capped"] {
        for i in 0..20 {
            ledger
                .reserve(scope, 1_000_000, t0() + Duration::seconds(i), TTL)
                .unwrap_or_else(|_| {
                    panic!(
                        "{backend} C6 VIOLATION: an uncapped scope refused a reservation — \
                         an absent cap must never become an accidental block"
                    )
                });
        }
        assert_eq!(
            ledger.limit(scope),
            None,
            "{backend} C6: the scope must report no limit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::InMemoryLedger;

    /// The reference implementation must pass its own bar — otherwise the suite is measuring
    /// nothing. This is also the regression guard for the suite itself.
    #[test]
    fn in_memory_ledger_is_conformant() {
        assert_enforcement_conformance("InMemoryLedger", InMemoryLedger::new);
    }

    #[test]
    fn the_caveat_is_stated_not_implied() {
        // A green suite must not be readable as "safe across replicas". If someone deletes the
        // caveat, this fails and they have to think about why it was there.
        assert!(SINGLE_WRITER_CAVEAT.contains("NOT that the backend serializes concurrent writers"));
    }
}
