use super::*;
use std::sync::{Arc, Barrier};

fn reserve(ledger: &mut SqliteLedger, scope: &str) -> u64 {
    match ledger
        .reserve_durable(scope, 100, OffsetDateTime::now_utc(), Duration::hours(1))
        .unwrap()
    {
        ReserveOutcome::Admitted(lease) => lease.id,
        _ => panic!("unexpected denial"),
    }
}

fn unwrap_receipt(outcome: SettlementOutcome) -> SettlementReceipt {
    match outcome {
        SettlementOutcome::Committed(receipt) | SettlementOutcome::AlreadyCommitted(receipt) => {
            receipt
        }
    }
}

fn count(ledger: &SqliteLedger) -> i64 {
    ledger
        .conn
        .query_row("SELECT COUNT(*) FROM budget_settlement_outbox", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn atomic_receipt_replay_and_conflict() {
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    let id = reserve(&mut ledger, "team");
    let original = unwrap_receipt(ledger.settle_with_evidence_durable("team", id, 42).unwrap());
    assert_eq!(original.receipt_id.len(), 64);
    assert_eq!(original.scope, "team");
    assert_eq!(original.charged_tokens, 42);
    assert_eq!(ledger.spent_durable("team").unwrap(), 42);
    assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
    assert_eq!(
        ledger.settle_with_evidence_durable("team", id, 42).unwrap(),
        SettlementOutcome::AlreadyCommitted(original.clone())
    );
    assert!(matches!(
        ledger.settle_with_evidence_durable("team", id, 43),
        Err(EvidenceError::ConflictingCharge)
    ));
    assert!(matches!(
        ledger.settle_with_evidence_durable("other", id, 42),
        Err(EvidenceError::WrongScope)
    ));
    ledger.settle_durable(id, 900).unwrap(); // Legacy callers cannot overwrite the receipt's charge.
    assert_eq!(ledger.spent_durable("team").unwrap(), 42);
    assert_eq!(count(&ledger), 1);
    let timestamp: i64 = ledger
        .conn
        .query_row(
            "SELECT settled_at FROM budget_reservation WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(timestamp, original.settled_at);
}

#[test]
fn missing_wrong_scope_legacy_and_reclaimed_are_explicit() {
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    let id = reserve(&mut ledger, "team");
    assert!(matches!(
        ledger.settle_with_evidence_durable("other", id, 2),
        Err(EvidenceError::WrongScope)
    ));
    assert!(matches!(
        ledger.settle_with_evidence_durable("team", id + 1, 2),
        Err(EvidenceError::MissingReservation)
    ));
    ledger.settle_durable(id, 2).unwrap();
    assert!(matches!(
        ledger.settle_with_evidence_durable("team", id, 2),
        Err(EvidenceError::LegacySettlement)
    ));
    let reclaimed = reserve(&mut ledger, "team");
    ledger
        .reclaim_expired_durable(OffsetDateTime::now_utc() + Duration::hours(2))
        .unwrap();
    assert!(matches!(
        ledger.settle_with_evidence_durable("team", reclaimed, 2),
        Err(EvidenceError::MissingReservation)
    ));
    assert_eq!(count(&ledger), 0);
}

#[test]
fn checked_integer_bounds_and_zero_charge() {
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    let id = reserve(&mut ledger, "team");
    for (lease, charge) in [(0, 1), (u64::MAX, 1), (id, i64::MAX as u64 + 1)] {
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", lease, charge),
            Err(EvidenceError::InvalidInput)
        ));
    }
    assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
    ledger.settle_with_evidence_durable("team", id, 0).unwrap();
    let id = reserve(&mut ledger, "team");
    ledger
        .settle_with_evidence_durable("team", id, i64::MAX as u64)
        .unwrap();
    assert_eq!(ledger.spent_durable("team").unwrap(), i64::MAX as u64);
    assert_eq!(count(&ledger), 2);
}

#[test]
fn failed_or_ignored_receipt_insert_rolls_back_charge() {
    for failure in ["RAISE(ABORT, 'injected')", "RAISE(IGNORE)"] {
        let mut ledger = SqliteLedger::open(":memory:").unwrap();
        let id = reserve(&mut ledger, "team");
        ledger.conn.execute_batch(&format!("CREATE TRIGGER reject_receipt BEFORE INSERT ON budget_settlement_outbox BEGIN SELECT {failure}; END;")).unwrap();
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", id, 42),
            Err(EvidenceError::Storage(_))
        ));
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(count(&ledger), 0);
        ledger
            .conn
            .execute_batch("DROP TRIGGER reject_receipt")
            .unwrap();
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", id, 42),
            Ok(SettlementOutcome::Committed(_))
        ));
    }
}

#[test]
fn ignored_settlement_cannot_create_receipt() {
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    let id = reserve(&mut ledger, "team");
    ledger.conn.execute_batch("CREATE TRIGGER reject_settle BEFORE UPDATE ON budget_reservation BEGIN SELECT RAISE(IGNORE); END;").unwrap();
    assert!(matches!(
        ledger.settle_with_evidence_durable("team", id, 42),
        Err(EvidenceError::Storage(_))
    ));
    assert_eq!(count(&ledger), 0);
    assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
}

#[test]
fn receipt_and_acknowledgement_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let path = path.to_str().unwrap();
    let mut ledger = SqliteLedger::open(path).unwrap();
    let id = reserve(&mut ledger, "team");
    let original = unwrap_receipt(ledger.settle_with_evidence_durable("team", id, 42).unwrap());
    drop(ledger);
    let mut ledger = SqliteLedger::open(path).unwrap();
    let now = OffsetDateTime::now_utc();
    let claims = ledger.claim_settlements_durable(10, now, 30).unwrap();
    assert_eq!(claims[0].receipt, original);
    assert!(ledger
        .acknowledge_settlement_durable(&original.receipt_id, &claims[0].claim_token, now)
        .unwrap());
    drop(ledger);
    let mut ledger = SqliteLedger::open(path).unwrap();
    assert!(ledger
        .claim_settlements_durable(10, now + Duration::hours(1), 30)
        .unwrap()
        .is_empty());
    assert_eq!(
        ledger.settle_with_evidence_durable("team", id, 42).unwrap(),
        SettlementOutcome::AlreadyCommitted(original)
    );
    assert_eq!(count(&ledger), 1);
}

#[test]
fn independent_connections_serialize_same_lease() {
    for conflicting in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db").to_str().unwrap().to_string();
        let mut ledger = SqliteLedger::open(&path).unwrap();
        let id = reserve(&mut ledger, "team");
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|i| {
                let mut connection = SqliteLedger::open(&path).unwrap();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    connection.settle_with_evidence_durable(
                        "team",
                        id,
                        if conflicting { 42 + i } else { 42 },
                    )
                })
            })
            .collect();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Ok(SettlementOutcome::Committed(_))))
                .count(),
            1
        );
        if conflicting {
            assert_eq!(
                results
                    .iter()
                    .filter(|r| matches!(r, Err(EvidenceError::ConflictingCharge)))
                    .count(),
                1
            );
        } else {
            assert_eq!(
                unwrap_receipt(results[0].as_ref().unwrap().clone()),
                unwrap_receipt(results[1].as_ref().unwrap().clone())
            );
        }
        assert_eq!(count(&ledger), 1);
        assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
    }
}

#[test]
fn bounded_claims_expiry_and_fenced_acknowledgement() {
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    let id = reserve(&mut ledger, "team");
    ledger.settle_with_evidence_durable("team", id, 42).unwrap();
    let now = OffsetDateTime::now_utc();
    for (limit, duration) in [(0, 1), (1001, 1), (1, 0), (1, 3601)] {
        assert!(matches!(
            ledger.claim_settlements_durable(limit, now, duration),
            Err(EvidenceError::InvalidInput)
        ));
    }
    let first = ledger
        .claim_settlements_durable(1, now, 10)
        .unwrap()
        .remove(0);
    assert!(ledger
        .claim_settlements_durable(1, now, 10)
        .unwrap()
        .is_empty());
    let expired = now + Duration::seconds(10);
    assert!(!ledger
        .acknowledge_settlement_durable(&first.receipt.receipt_id, &first.claim_token, expired)
        .unwrap());
    let second = ledger
        .claim_settlements_durable(1, expired, 10)
        .unwrap()
        .remove(0);
    assert_eq!(first.receipt, second.receipt);
    assert_ne!(first.claim_token, second.claim_token);
    assert!(!ledger
        .acknowledge_settlement_durable(&first.receipt.receipt_id, &first.claim_token, expired)
        .unwrap());
    assert!(!ledger
        .acknowledge_settlement_durable("unknown", &second.claim_token, expired)
        .unwrap());
    assert!(ledger
        .acknowledge_settlement_durable(&second.receipt.receipt_id, &second.claim_token, expired)
        .unwrap());
    assert!(!ledger
        .acknowledge_settlement_durable(&second.receipt.receipt_id, &second.claim_token, expired)
        .unwrap());
    assert!(ledger
        .claim_settlements_durable(1, expired + Duration::hours(1), 10)
        .unwrap()
        .is_empty());
    assert_eq!(count(&ledger), 1);
}

#[test]
fn independent_claim_workers_take_disjoint_bounded_batches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db").to_str().unwrap().to_string();
    let mut ledger = SqliteLedger::open(&path).unwrap();
    for _ in 0..10 {
        let id = reserve(&mut ledger, "team");
        ledger.settle_with_evidence_durable("team", id, 1).unwrap();
    }
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let mut connection = SqliteLedger::open(&path).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                connection
                    .claim_settlements_durable(5, OffsetDateTime::now_utc(), 60)
                    .unwrap()
            })
        })
        .collect();
    let ids: Vec<_> = workers
        .into_iter()
        .flat_map(|worker| {
            let batch = worker.join().unwrap();
            assert_eq!(batch.len(), 5);
            batch.into_iter().map(|claim| claim.receipt.receipt_id)
        })
        .collect();
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        10
    );
}

#[test]
fn shard_local_lease_ids_have_distinct_evidence_and_checked_locations() {
    let sharded = crate::ShardedLedger::open_sharded(":memory:", 2).unwrap();
    let now = OffsetDateTime::now_utc();
    // FNV-1a maps adjacent final bytes to opposite shards for N=2.
    for scope in ["team-a", "team-b"] {
        let lease = match sharded
            .reserve_durable(scope, 100, now, Duration::hours(1))
            .unwrap()
        {
            ReserveOutcome::Admitted(lease) => lease,
            _ => panic!("denied"),
        };
        assert_eq!(lease.id, 1);
        sharded
            .settle_with_evidence_durable(scope, lease.id, 42)
            .unwrap();
    }
    assert_eq!(sharded.evidence_shard_count(), 2);
    let first = sharded
        .claim_settlements_durable(0, 10, now, 60)
        .unwrap()
        .remove(0);
    let second = sharded
        .claim_settlements_durable(1, 10, now, 60)
        .unwrap()
        .remove(0);
    assert_ne!(first.receipt.receipt_id, second.receipt.receipt_id);
    assert!(!sharded
        .acknowledge_settlement_durable(1, &first.receipt.receipt_id, &first.claim_token, now)
        .unwrap());
    assert!(sharded
        .acknowledge_settlement_durable(0, &first.receipt.receipt_id, &first.claim_token, now)
        .unwrap());
    assert!(matches!(
        sharded.claim_settlements_durable(2, 1, now, 1),
        Err(EvidenceError::InvalidShard)
    ));
    assert!(matches!(
        sharded.acknowledge_settlement_durable(2, "a", "b", now),
        Err(EvidenceError::InvalidShard)
    ));
}

#[test]
fn legacy_migration_refuses_pending_and_acknowledged_evidence_without_file_changes() {
    for acknowledge in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db").to_str().unwrap().to_string();
        let mut ledger = SqliteLedger::open(&path).unwrap();
        let id = reserve(&mut ledger, "team");
        let original = unwrap_receipt(ledger.settle_with_evidence_durable("team", id, 42).unwrap());
        if acknowledge {
            let now = OffsetDateTime::now_utc();
            let claim = ledger
                .claim_settlements_durable(1, now, 60)
                .unwrap()
                .remove(0);
            ledger
                .acknowledge_settlement_durable(&claim.receipt.receipt_id, &claim.claim_token, now)
                .unwrap();
        }
        drop(ledger);
        assert!(crate::ShardedLedger::open_sharded(&path, 2).is_err());
        for shard in 0..2 {
            assert!(!std::path::Path::new(&format!("{path}-ledger-shard-{shard}.db")).exists());
        }
        let mut ledger = SqliteLedger::open(&path).unwrap();
        assert_eq!(ledger.spent_durable("team").unwrap(), 42);
        assert_eq!(count(&ledger), 1);
        assert_eq!(
            ledger.settle_with_evidence_durable("team", id, 42).unwrap(),
            SettlementOutcome::AlreadyCommitted(original)
        );
    }
}

#[test]
fn failed_claim_batch_rolls_back_all_claims() {
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    for _ in 0..2 {
        let id = reserve(&mut ledger, "team");
        ledger.settle_with_evidence_durable("team", id, 1).unwrap();
    }
    // Reject the second row in polling order, after the first update has succeeded.
    ledger
        .conn
        .execute_batch(
            "CREATE TRIGGER reject_claim BEFORE UPDATE OF claim_token ON budget_settlement_outbox
         WHEN OLD.receipt_id = (SELECT receipt_id FROM budget_settlement_outbox
             ORDER BY settled_at, receipt_id LIMIT 1 OFFSET 1)
         BEGIN SELECT RAISE(IGNORE); END;",
        )
        .unwrap();
    let now = OffsetDateTime::now_utc();
    assert!(matches!(
        ledger.claim_settlements_durable(2, now, 30),
        Err(EvidenceError::Storage(_))
    ));
    let claimed: i64 = ledger
        .conn
        .query_row(
            "SELECT COUNT(*) FROM budget_settlement_outbox WHERE claim_token IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(claimed, 0);
    ledger
        .conn
        .execute_batch("DROP TRIGGER reject_claim")
        .unwrap();
    assert_eq!(
        ledger.claim_settlements_durable(2, now, 30).unwrap().len(),
        2
    );
}

// Invoked only by the parent test with an isolated temporary DB. process::exit bypasses
// Rust destructors/SQLite connection close; this is process-crash, not power-loss evidence.
#[test]
fn process_exit_helper() {
    let Ok(path) = std::env::var("SANDHI_EVIDENCE_CRASH_TEST_DB") else {
        return;
    };
    let mode = std::env::var("SANDHI_EVIDENCE_CRASH_TEST_MODE").unwrap();
    let mut ledger = SqliteLedger::open(&path).unwrap();
    if mode == "committed" {
        ledger.settle_with_evidence_durable("team", 1, 42).unwrap();
    } else {
        // Stage the same two storage facts, then exit without COMMIT or rollback.
        // Injection-trigger tests above exercise rollback through the production API.
        ledger
            .conn
            .execute_batch(
                "BEGIN IMMEDIATE;
             UPDATE budget_reservation SET actual = 42, settled = 1, settled_at = 1 WHERE id = 1;
             INSERT INTO budget_settlement_outbox
                 (receipt_id, reservation_id, scope, charged_tokens, settled_at)
                 VALUES ('uncommitted-test-receipt', 1, 'team', 42, 1);",
            )
            .unwrap();
    }
    std::process::exit(77);
}

#[test]
fn process_exit_preserves_committed_pair_and_discards_uncommitted_pair() {
    for mode in ["committed", "uncommitted"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        assert_eq!(reserve(&mut ledger, "team"), 1);
        drop(ledger);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ledger::evidence::tests::process_exit_helper",
                "--nocapture",
            ])
            .env("SANDHI_EVIDENCE_CRASH_TEST_DB", &path)
            .env("SANDHI_EVIDENCE_CRASH_TEST_MODE", mode)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77));
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        if mode == "committed" {
            assert_eq!(ledger.spent_durable("team").unwrap(), 42);
            assert_eq!(count(&ledger), 1);
            assert!(matches!(
                ledger.settle_with_evidence_durable("team", 1, 42),
                Ok(SettlementOutcome::AlreadyCommitted(_))
            ));
        } else {
            assert_eq!(ledger.spent_durable("team").unwrap(), 0);
            assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
            assert_eq!(count(&ledger), 0);
            assert!(matches!(
                ledger.settle_with_evidence_durable("team", 1, 42),
                Ok(SettlementOutcome::Committed(_))
            ));
        }
    }
}
