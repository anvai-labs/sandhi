//! Per-virtual-key request rate limiting (TD-0012 P1).
//!
//! `rate_limit_per_min` has been accepted by the CLI, persisted, and returned by the admin API
//! since TD-0003 — and read nowhere. An operator could set a limit, see it echoed back, and be
//! told nothing when it did not apply. This closes that.
//!
//! **Token bucket, not a per-minute counter** (D1). A fixed window admits `2 × limit` across a
//! boundary — 60 requests at 11:59:59 and 60 more at 12:00:00 — which is unacceptable for a control
//! whose whole purpose is bounding a runaway. The bucket refills continuously at `limit / 60` per
//! second with capacity `limit`, so a minute's worth of burst is allowed (deliberate: a limit that
//! refused the second request of a batch would break every batching client) but the *sustained*
//! rate cannot exceed the limit.
//!
//! **In-memory** (D2). This is a per-request decision at request frequency; putting it on the
//! durable path would add a write per call for a control that is disposable by nature. The
//! consequence is stated rather than buried: with N replicas the effective limit is `N × limit`,
//! the same single-node limitation the enforcement ledger has (TD-0007).
//!
//! The clock is monotonic (`Instant`), never wall-clock: an NTP step backwards must not hand a
//! caller free capacity.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a key's bucket survives without traffic before eviction (D3).
///
/// An order of magnitude longer than the one-minute refill window on purpose: if this were shorter
/// than a refill, an idle-then-returning caller would be handed a full bucket early and could
/// exceed its limit. Ten minutes is comfortably past a full refill while still bounding state to
/// *recently active* keys rather than every key ever seen.
const IDLE_EVICTION: Duration = Duration::from_secs(600);

/// Buckets are pruned opportunistically rather than by a background task — one sweep per this many
/// checks keeps it O(1) amortised and avoids a timer that has to be shut down cleanly.
const SWEEP_EVERY: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    /// Refused, with whole seconds until the next token is available.
    ///
    /// Whole seconds because that is what `Retry-After` carries and what SDK backoff consumes;
    /// floored at 1 so a client never reads `0` and retries in a tight loop.
    Limited {
        retry_after_secs: u64,
    },
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Per-key token buckets.
#[derive(Debug, Default)]
pub struct RateLimiter {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    buckets: HashMap<String, Bucket>,
    checks: u64,
}

impl RateLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether `key` may proceed under `limit_per_min`.
    ///
    /// `None` or `0` means unlimited — the field is optional, and an absent limit must never
    /// become an accidental block.
    pub fn check(&self, key: &str, limit_per_min: Option<u32>) -> Decision {
        self.check_at(key, limit_per_min, Instant::now())
    }

    /// Injectable clock, so the tests can prove refill and eviction without sleeping.
    fn check_at(&self, key: &str, limit_per_min: Option<u32>, now: Instant) -> Decision {
        let Some(limit) = limit_per_min.filter(|l| *l > 0) else {
            return Decision::Allowed;
        };
        let capacity = f64::from(limit);
        let per_second = capacity / 60.0;

        let Ok(mut inner) = self.inner.lock() else {
            // A poisoned lock must not become an outage: fail OPEN. A rate limiter is a protection
            // against runaway callers, not a correctness boundary like the budget ledger — refusing
            // all traffic because a mutex broke would be the worse failure.
            return Decision::Allowed;
        };

        inner.checks = inner.checks.wrapping_add(1);
        if inner.checks % SWEEP_EVERY == 0 {
            inner
                .buckets
                .retain(|_, b| now.duration_since(b.last) < IDLE_EVICTION);
        }

        let bucket = inner.buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });

        // Refill for elapsed time, capped at capacity.
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * per_second).min(capacity);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Decision::Allowed
        } else {
            let deficit = 1.0 - bucket.tokens;
            let wait = deficit / per_second;
            Decision::Limited {
                retry_after_secs: (wait.ceil() as u64).max(1),
            }
        }
    }

    /// Drop a key's bucket — used when a key is revoked, so state follows the key's lifetime.
    pub fn forget(&self, key: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.buckets.remove(key);
        }
    }

    #[cfg(test)]
    fn tracked_keys(&self) -> usize {
        self.inner.lock().map(|i| i.buckets.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_or_zero_limit_is_unlimited() {
        let limiter = RateLimiter::new();
        for _ in 0..1_000 {
            assert_eq!(limiter.check("vk", None), Decision::Allowed);
            assert_eq!(limiter.check("vk", Some(0)), Decision::Allowed);
        }
    }

    #[test]
    fn admits_a_full_minute_of_burst_then_refuses() {
        let limiter = RateLimiter::new();
        let now = Instant::now();
        for i in 0..60 {
            assert_eq!(
                limiter.check_at("vk", Some(60), now),
                Decision::Allowed,
                "request {i} of the burst should be admitted"
            );
        }
        match limiter.check_at("vk", Some(60), now) {
            Decision::Limited { retry_after_secs } => {
                // One token at 1/sec, floored at 1 — never 0, which would invite a tight retry.
                assert_eq!(retry_after_secs, 1);
            }
            Decision::Allowed => panic!("the 61st request in the same instant must be refused"),
        }
    }

    #[test]
    fn refills_continuously_rather_than_at_a_window_boundary() {
        // The defect a fixed window has: 2x the limit across a boundary. Here, draining the bucket
        // and waiting half a minute must return exactly half the capacity, not all of it.
        let limiter = RateLimiter::new();
        let start = Instant::now();
        for _ in 0..60 {
            assert_eq!(limiter.check_at("vk", Some(60), start), Decision::Allowed);
        }
        let half_minute_later = start + Duration::from_secs(30);
        let mut admitted = 0;
        for _ in 0..60 {
            if limiter.check_at("vk", Some(60), half_minute_later) == Decision::Allowed {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 30, "30s at 1/sec must refill exactly 30 tokens");
    }

    #[test]
    fn a_returning_key_cannot_exceed_its_limit_via_eviction() {
        // D3's hazard: if eviction were shorter than a refill window, an idle caller would come
        // back to a full bucket early. Drain, jump just past eviction, and confirm the caller gets
        // capacity it had legitimately earned by waiting — never more.
        let limiter = RateLimiter::new();
        let start = Instant::now();
        for _ in 0..60 {
            limiter.check_at("vk", Some(60), start);
        }
        let after_eviction = start + IDLE_EVICTION + Duration::from_secs(1);
        let mut admitted = 0;
        for _ in 0..100 {
            if limiter.check_at("vk", Some(60), after_eviction) == Decision::Allowed {
                admitted += 1;
            }
        }
        // Ten idle minutes earns a full bucket either way — the point is it is never MORE than
        // capacity, whether the bucket was evicted and recreated or simply refilled.
        assert_eq!(admitted, 60);
    }

    #[test]
    fn idle_buckets_are_evicted_so_state_tracks_live_keys() {
        let limiter = RateLimiter::new();
        let start = Instant::now();
        for i in 0..10 {
            limiter.check_at(&format!("vk{i}"), Some(60), start);
        }
        assert_eq!(limiter.tracked_keys(), 10);

        // Drive enough checks on ONE key, well past the idle window, to trigger a sweep.
        let later = start + IDLE_EVICTION + Duration::from_secs(1);
        for _ in 0..SWEEP_EVERY {
            limiter.check_at("vk_active", Some(6_000), later);
        }
        assert_eq!(
            limiter.tracked_keys(),
            1,
            "only the active key should survive the sweep"
        );
    }

    #[test]
    fn keys_are_independent() {
        let limiter = RateLimiter::new();
        let now = Instant::now();
        for _ in 0..60 {
            limiter.check_at("noisy", Some(60), now);
        }
        assert!(matches!(
            limiter.check_at("noisy", Some(60), now),
            Decision::Limited { .. }
        ));
        assert_eq!(
            limiter.check_at("quiet", Some(60), now),
            Decision::Allowed,
            "one key exhausting its bucket must not affect another"
        );
    }

    #[test]
    fn forget_drops_a_revoked_keys_state() {
        let limiter = RateLimiter::new();
        let now = Instant::now();
        limiter.check_at("vk", Some(60), now);
        assert_eq!(limiter.tracked_keys(), 1);
        limiter.forget("vk");
        assert_eq!(limiter.tracked_keys(), 0);
    }
}
