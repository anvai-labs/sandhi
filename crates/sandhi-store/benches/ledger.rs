//! TD-0015 P1: the enforcement-ledger benchmark.
//!
//! Measures what ADR-0006's F1 inference asserts — that the ledger's
//! serialized, durable commit path (not byte movement) is the throughput
//! ceiling — across the three levers the design identifies: the durability
//! level (`synchronous`), batching, and thread count. The multi-threaded
//! harness is deliberately crude wall-clock (std threads over the shared
//! `Mutex<SqliteLedger>`): the question is end-to-end ops/s through the real
//! ledger API under contention, not a micro-measurement of any single call.
//!
//! Run: `cargo bench -p sandhi-store`. Recorded numbers live in
//! `docs/td/TD-0015-performance-baseline-and-fault-injection.md`.

use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion};
use sandhi_store::{SqliteLedger, Synchronous};
use time::Duration;
use time::OffsetDateTime;

/// One admission: reserve a ceiling, then settle it by lease id — the exact
/// two durable writes every metered call makes. No budget row is installed,
/// so every reserve is admitted (the cap check is ADR-0005's business, not
/// the commit path's cost).
fn round_trip(ledger: &mut SqliteLedger) {
    let ttl = Duration::seconds(900);
    match ledger
        .reserve_durable("bench-scope", 1000, OffsetDateTime::now_utc(), ttl)
        .unwrap()
    {
        sandhi_store::ReserveOutcome::Admitted(reservation) => {
            ledger.settle_durable(reservation.id, 1000).unwrap();
        }
        sandhi_store::ReserveOutcome::Denied(_) => unreachable!("no cap installed"),
    }
}

fn open(dir: &tempfile::TempDir, sync: Synchronous) -> SqliteLedger {
    SqliteLedger::open_with_synchronous(dir.path().join("l.db").to_str().unwrap(), sync).unwrap()
}

fn bench_reserve_settle(c: &mut Criterion) {
    let mut group = c.benchmark_group("ledger/reserve-settle");
    group.throughput(criterion::Throughput::Elements(1));

    for (label, sync) in [
        ("synchronous=FULL", Synchronous::Full),
        ("synchronous=NORMAL", Synchronous::Normal),
    ] {
        group.bench_function(label, |b| {
            let dir = tempfile::tempdir().unwrap();
            let mut ledger = open(&dir, sync);
            b.iter(|| round_trip(&mut ledger));
        });
    }
    group.finish();
}

// The group-commit upper bound (K admissions per transaction) is deliberately
// NOT measured here: a true batch needs the group-commit API that TD-0016 P3
// would introduce, and a bench-only loop of per-op transactions just re-measures
// `reserve-settle` under a different name. Measured 2026-09-01 on macOS/APFS:
// sequential FULL round-trips ran ~0.2-0.9 ms/op with a heavy tail — the
// per-op number is machine-specific (APFS fsync semantics), directional only.
/// Contention: N worker threads driving one shared `Mutex<SqliteLedger>` —
/// today's architecture — measuring wall-clock through the real API under
/// thread contention. The delta between thread counts is the
/// serialization split TD-0016 P1 needs.
fn bench_threaded(c: &mut Criterion) {
    let mut group = c.benchmark_group("ledger/threaded");
    for threads in [1usize, 4] {
        group.throughput(criterion::Throughput::Elements(200 * threads as u64));
        group.bench_function(criterion::BenchmarkId::new("ops", threads), |b| {
            let dir = tempfile::tempdir().unwrap();
            let ledger = open(&dir, Synchronous::Full);
            let shared = std::sync::Arc::new(std::sync::Mutex::new(ledger));
            b.iter_custom(|iters| {
                let start = Instant::now();
                std::thread::scope(|scope| {
                    for _ in 0..threads {
                        let shared = std::sync::Arc::clone(&shared);
                        scope.spawn(move || {
                            for _ in 0..iters {
                                let mut ledger = shared.lock().unwrap();
                                round_trip(&mut ledger);
                            }
                        });
                    }
                });
                start.elapsed()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_reserve_settle, bench_threaded);
criterion_main!(benches);
