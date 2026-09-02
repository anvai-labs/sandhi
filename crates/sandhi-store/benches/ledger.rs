//! TD-0015 P1 + TD-0016 P1: the enforcement-ledger benchmark.
//!
//! Measures what ADR-0006's F1 inference asserts — that the ledger's
//! serialized, durable commit path (not byte movement) is the throughput
//! ceiling — across the levers the design identifies: durability level,
//! thread count, and (TD-0016 P1) scope sharding.
//!
//! Accounting discipline: every timed call performs a FIXED total number of
//! admissions (`TOTAL_OPS`, split across threads/shards), and throughput is
//! declared as that fixed total. An earlier shape let each thread loop
//! `iters` times while declaring a fixed element count — the reported times
//! were then threads-dependent artifacts, and the "1.26× scaling" conclusion
//! drawn from them was retracted.
//!
//! Absolute numbers are macOS/APFS-local and directional; the cross-configuration
//! ratios are the load-bearing results.
//!
//! Run: `cargo bench -p sandhi-store`. Recorded numbers live in
//! `docs/td/TD-0015-performance-baseline-and-fault-injection.md` and
//! `docs/td/TD-0016-enforcement-throughput-ceiling.md`.

use std::time::Instant;

use criterion::{criterion_group, criterion_main, Criterion};
use sandhi_store::{SqliteLedger, Synchronous};
use time::Duration;
use time::OffsetDateTime;

const TOTAL_OPS: u64 = 4_000;

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

/// The durability lever: FULL (production) vs NORMAL. Identical code, one
/// pragma — the delta is pure fsync cost.
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

/// The contention lever, single-file (pre-P1 architecture): N threads on one
/// shared `Mutex<SqliteLedger>`. FIXED total work per timed call.
fn bench_threaded(c: &mut Criterion) {
    let mut group = c.benchmark_group("ledger/threaded");
    group.throughput(criterion::Throughput::Elements(TOTAL_OPS));
    for threads in [1usize, 4] {
        group.bench_function(criterion::BenchmarkId::new("ops", threads), |b| {
            let dir = tempfile::tempdir().unwrap();
            let ledger = open(&dir, Synchronous::Full);
            let shared = std::sync::Arc::new(std::sync::Mutex::new(ledger));
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    std::thread::scope(|scope| {
                        for _ in 0..threads {
                            let shared = std::sync::Arc::clone(&shared);
                            scope.spawn(move || {
                                let mut ledger = shared.lock().unwrap();
                                for _ in 0..(TOTAL_OPS / threads as u64) {
                                    round_trip(&mut ledger);
                                }
                            });
                        }
                    });
                }
                start.elapsed()
            });
        }); // <- |b|{ closes here; the ) closes bench_function(
    }
    group.finish();
}

/// TD-0016 P1 acceptance: N scopes on N shards — cross-scope durable commits
/// in parallel. Compare against `ledger/threaded` at the same wall
/// conditions: the delta is the parallelism the sharding buys.
fn bench_sharded(c: &mut Criterion) {
    let mut group = c.benchmark_group("ledger/sharded");
    group.throughput(criterion::Throughput::Elements(TOTAL_OPS));
    for shards in [1usize, 4] {
        group.bench_function(criterion::BenchmarkId::new("ops", shards), |b| {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().join("l.db");
            let ledger =
                sandhi_store::ShardedLedger::open_sharded(base.to_str().unwrap(), shards).unwrap();
            let shared = std::sync::Arc::new(std::sync::Mutex::new(ledger));
            b.iter_custom(|iters| {
                let start = Instant::now();
                for _ in 0..iters {
                    std::thread::scope(|scope| {
                        for s in 0..shards {
                            let shared = std::sync::Arc::clone(&shared);
                            let scope_name = format!("group:shard-{s}");
                            scope.spawn(move || {
                                let ledger = shared.lock().unwrap();
                                for _ in 0..(TOTAL_OPS / shards as u64) {
                                    match ledger
                                        .reserve_durable(
                                            &scope_name,
                                            1000,
                                            OffsetDateTime::now_utc(),
                                            Duration::seconds(900),
                                        )
                                        .unwrap()
                                    {
                                        sandhi_store::ReserveOutcome::Admitted(r) => {
                                            ledger.settle_durable(&scope_name, r.id, 1000).unwrap();
                                        }
                                        other => unreachable!("no cap installed: {other:?}"),
                                    }
                                }
                            });
                        }
                    });
                }
                start.elapsed()
            });
        }); // <- |b|{ closes here; the ) closes bench_sharded(
    }
    group.finish();
}

criterion_group!(benches, bench_reserve_settle, bench_threaded, bench_sharded);
criterion_main!(benches);
