use super::*;
use sandhi_core::{
    CacheReadObservation, CacheReadSource, CacheReadStatus, UsageBasis, UsageCompleteness,
};
use std::sync::{Arc, Barrier};

fn tracked(ledger: &mut SqliteLedger) -> ExecutionIntent {
    let IntentAdmission::Admitted(intent) = ledger
        .reserve_with_intent_durable(
            "team",
            100,
            OffsetDateTime::now_utc(),
            Duration::seconds(1),
            10,
        )
        .unwrap()
    else {
        panic!("denied")
    };
    intent
}

fn observed_usage() -> UsageV2 {
    UsageV2 {
        tokens_in: 10,
        tokens_out: 20,
        cache_creation_tokens: 3,
        cache_read_tokens: 4,
        cache_read_observation: Some(CacheReadObservation::origin(CacheReadStatus::Reported)),
        audio_input_tokens: Some(2),
        audio_output_tokens: Some(3),
        reasoning_tokens: Some(5),
        reasoning_included: Some(false),
        accepted_prediction_tokens: Some(6),
        rejected_prediction_tokens: Some(7),
        completeness: UsageCompleteness::Final,
        basis: UsageBasis::ProviderReported,
        attempts: 1,
        outcome: Some("success".into()),
        upstream_request_id: Some("upstream-1".into()),
        duration_ms: Some(123),
        duration_source: Some(sandhi_core::LatencySource::Origin),
        time_to_first_token_ms: Some(10),
        time_to_first_token_source: Some(sandhi_core::LatencySource::Boundary),
    }
}

fn unwrap_observation(outcome: ObservationOutcome) -> TerminalObservation {
    match outcome {
        ObservationOutcome::Recorded(value) | ObservationOutcome::AlreadyRecorded(value) => value,
    }
}

#[test]
fn terminal_snapshot_is_immutable_and_survives_reopen_and_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("terminal.db");
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let intent = tracked(&mut ledger);
    assert_eq!(
        ledger
            .terminal_durable("team", &intent.execution_id)
            .unwrap(),
        None
    );
    let usage = observed_usage();
    let original = unwrap_observation(
        ledger
            .record_terminal_durable("team", &intent.execution_id, &usage)
            .unwrap(),
    );
    assert_eq!(original.version, 1);
    assert_eq!(original.usage, usage);
    assert_eq!(original.frozen_charge, Some(42));
    assert_eq!(original.reservation_id, intent.reservation.id);
    assert_eq!(original.scope, "team");
    assert_eq!(ledger.spent_durable("team").unwrap(), 0);
    assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
    drop(ledger);
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    assert_eq!(
        ledger
            .terminal_durable("team", &intent.execution_id)
            .unwrap(),
        Some(original.clone())
    );
    for changed in [
        UsageV2 {
            outcome: Some("error".into()),
            ..usage.clone()
        },
        UsageV2 {
            basis: UsageBasis::Estimated,
            ..usage.clone()
        },
        UsageV2 {
            completeness: UsageCompleteness::Partial,
            ..usage.clone()
        },
        UsageV2 {
            tokens_in: 11,
            tokens_out: 19,
            ..usage.clone()
        },
    ] {
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &changed),
            Err(EvidenceError::ConflictingObservation)
        ));
    }
    assert!(matches!(
        ledger.record_terminal_durable("other", &intent.execution_id, &usage),
        Err(EvidenceError::WrongScope)
    ));
    assert!(matches!(
        ledger.terminal_durable("other", &intent.execution_id),
        Err(EvidenceError::WrongScope)
    ));
    ledger
        .settle_terminal_durable("team", &intent.execution_id)
        .unwrap();
    assert_eq!(
        ledger
            .record_terminal_durable("team", &intent.execution_id, &usage)
            .unwrap(),
        ObservationOutcome::AlreadyRecorded(original)
    );
}

#[test]
fn terminal_unknown_zero_partial_and_reasoning_keep_their_meanings() {
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    for (usage, charge) in [
        (UsageV2::default(), None),
        (
            UsageV2 {
                completeness: UsageCompleteness::Final,
                ..UsageV2::default()
            },
            Some(0),
        ),
        (
            UsageV2 {
                completeness: UsageCompleteness::Partial,
                basis: UsageBasis::Estimated,
                ..observed_usage()
            },
            Some(42),
        ),
        (
            UsageV2 {
                reasoning_included: Some(true),
                ..observed_usage()
            },
            Some(37),
        ),
    ] {
        let intent = tracked(&mut ledger);
        let value = unwrap_observation(
            ledger
                .record_terminal_durable("team", &intent.execution_id, &usage)
                .unwrap(),
        );
        assert_eq!(value.usage, usage);
        assert_eq!(value.frozen_charge, charge);
        ledger
            .reclaim_expired_durable(OffsetDateTime::now_utc() + Duration::hours(2))
            .unwrap();
        assert_eq!(
            ledger
                .terminal_durable("team", &intent.execution_id)
                .unwrap(),
            Some(value)
        );
    }
    assert_eq!(ledger.reserved_durable("team").unwrap(), 400);
    assert_eq!(ledger.spent_durable("team").unwrap(), 0);
}

#[test]
fn terminal_missing_settled_invalid_and_lossy_input_are_rejected() {
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    let intent = tracked(&mut ledger);
    for usage in [
        UsageV2 {
            tokens_in: i64::MAX as u64,
            ..observed_usage()
        },
        UsageV2 {
            outcome: Some("x".repeat(257)),
            ..observed_usage()
        },
        UsageV2 {
            upstream_request_id: Some("x".repeat(1025)),
            ..observed_usage()
        },
        UsageV2 {
            cache_read_observation: Some(CacheReadObservation {
                status: CacheReadStatus::Unsupported,
                source: CacheReadSource::OriginUsage,
            }),
            ..observed_usage()
        },
    ] {
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &usage),
            Err(EvidenceError::InvalidInput)
        ));
    }
    for id in ["bad".to_string(), "z".repeat(64)] {
        assert!(matches!(
            ledger.record_terminal_durable("team", &id, &observed_usage()),
            Err(EvidenceError::InvalidInput)
        ));
        assert!(matches!(
            ledger.terminal_durable("team", &id),
            Err(EvidenceError::InvalidInput)
        ));
    }
    assert!(matches!(
        ledger.record_terminal_durable("team", &"0".repeat(64), &observed_usage()),
        Err(EvidenceError::MissingIntent)
    ));
    assert_eq!(
        ledger.terminal_durable("team", &"0".repeat(64)).unwrap(),
        None
    );
    assert_eq!(
        ledger
            .terminal_durable("team", &intent.execution_id)
            .unwrap(),
        None
    );
    // Historical state from a writer predating tracked-settlement enforcement.
    ledger
        .conn
        .execute(
            "UPDATE budget_reservation SET actual = 7, settled = 1 WHERE id = ?1",
            [intent.reservation.id],
        )
        .unwrap();
    assert!(matches!(
        ledger.record_terminal_durable("team", &intent.execution_id, &observed_usage()),
        Err(EvidenceError::AlreadySettled)
    ));
}

#[test]
fn terminal_failed_or_ignored_insert_leaves_liability_and_can_retry() {
    for failure in ["RAISE(ABORT, 'injected')", "RAISE(IGNORE)"] {
        let mut ledger = SqliteLedger::open(":memory:").unwrap();
        let intent = tracked(&mut ledger);
        ledger.conn.execute_batch(&format!("CREATE TRIGGER reject_terminal BEFORE INSERT ON budget_terminal_observation BEGIN SELECT {failure}; END;")).unwrap();
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &observed_usage()),
            Err(EvidenceError::Storage(_))
        ));
        assert_eq!(
            ledger
                .terminal_durable("team", &intent.execution_id)
                .unwrap(),
            None
        );
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        ledger
            .conn
            .execute_batch("DROP TRIGGER reject_terminal")
            .unwrap();
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &observed_usage()),
            Ok(ObservationOutcome::Recorded(_))
        ));
    }
}

#[test]
fn terminal_independent_connections_have_one_immutable_winner() {
    for conflicting in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.db");
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        let intent = tracked(&mut ledger);
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|i| {
                let mut connection = SqliteLedger::open(path.to_str().unwrap()).unwrap();
                let id = intent.execution_id.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut usage = observed_usage();
                    if conflicting {
                        usage.tokens_in += i;
                    }
                    barrier.wait();
                    connection.record_terminal_durable("team", &id, &usage)
                })
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Ok(ObservationOutcome::Recorded(_))))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(EvidenceError::ConflictingObservation)))
                .count(),
            usize::from(conflicting)
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Ok(ObservationOutcome::AlreadyRecorded(_))))
                .count(),
            usize::from(!conflicting)
        );
        assert_eq!(
            ledger
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM budget_terminal_observation",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
    }
}

#[test]
fn terminal_read_rejects_unsupported_or_corrupt_snapshots_without_rewriting() {
    for mutation in [
        "UPDATE budget_terminal_observation SET version = 2",
        "UPDATE budget_terminal_observation SET usage_json = '{invalid'",
        "UPDATE budget_terminal_observation SET usage_json = json_set(usage_json, '$.future_field', 1)",
        "UPDATE budget_terminal_observation SET usage_json = json_set(usage_json, '$.cache_read_observation.status', 'future')",
        "UPDATE budget_terminal_observation SET usage_json = printf('%20000s', 'x')",
        "UPDATE budget_terminal_observation SET frozen_charge = NULL",
        "DELETE FROM budget_reservation",
        "DELETE FROM budget_execution_intent",
    ] {
        let mut ledger = SqliteLedger::open(":memory:").unwrap();
        let intent = tracked(&mut ledger);
        ledger
            .record_terminal_durable("team", &intent.execution_id, &observed_usage())
            .unwrap();
        ledger.conn.execute_batch(&format!("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON; {mutation}")).unwrap();
        let reserved_after_mutation = ledger.reserved_durable("team").unwrap();
        assert!(matches!(
            ledger.terminal_durable("team", &intent.execution_id),
            Err(EvidenceError::CorruptObservation)
        ));
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &observed_usage()),
            Err(EvidenceError::CorruptObservation)
        ));
        assert_eq!(ledger.reserved_durable("team").unwrap(), reserved_after_mutation);
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
    }
}

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
    if mode.starts_with("terminal_") || mode == "settlement_committed" {
        let execution_id: String = ledger
            .conn
            .query_row(
                "SELECT execution_id FROM budget_execution_intent",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if mode == "settlement_committed" {
            ledger
                .settle_terminal_durable("team", &execution_id)
                .unwrap();
        } else if mode == "terminal_committed" {
            ledger
                .record_terminal_durable("team", &execution_id, &observed_usage())
                .unwrap();
        } else {
            ledger.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            ledger.conn.execute("INSERT INTO budget_terminal_observation (execution_id, version, usage_json, frozen_charge, observed_at) VALUES (?1, 1, ?2, 42, 1)", params![execution_id, serde_json::to_string(&observed_usage()).unwrap()]).unwrap();
        }
    } else if mode == "intent_committed" {
        ledger
            .reserve_with_intent_durable(
                "team",
                7,
                OffsetDateTime::now_utc(),
                Duration::seconds(1),
                2,
            )
            .unwrap();
    } else if mode == "intent_uncommitted" {
        ledger
            .conn
            .execute_batch(
                "BEGIN IMMEDIATE;
            INSERT INTO budget_reservation (scope, ceiling, expires_at) VALUES ('team', 7, 1);
            INSERT INTO budget_execution_intent (execution_id, reservation_id, created_at)
                VALUES ('uncommitted', last_insert_rowid(), 1);",
            )
            .unwrap();
    } else if mode == "committed" {
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
    for mode in [
        "committed",
        "uncommitted",
        "intent_committed",
        "intent_uncommitted",
        "terminal_committed",
        "terminal_uncommitted",
        "settlement_committed",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        let terminal_id = if mode.starts_with("terminal_") || mode == "settlement_committed" {
            let intent = tracked(&mut ledger);
            if mode == "settlement_committed" {
                ledger
                    .record_terminal_durable("team", &intent.execution_id, &observed_usage())
                    .unwrap();
            }
            Some(intent.execution_id)
        } else {
            assert_eq!(reserve(&mut ledger, "team"), 1);
            None
        };
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
        if let Some(execution_id) = terminal_id {
            let observation = ledger.terminal_durable("team", &execution_id).unwrap();
            let settled = mode == "settlement_committed";
            assert_eq!(
                observation.is_some(),
                mode == "terminal_committed" || settled
            );
            if let Some(observation) = observation {
                assert_eq!(observation.usage, observed_usage());
                assert_eq!(observation.frozen_charge, Some(42));
            }
            assert_eq!(
                ledger.reserved_durable("team").unwrap(),
                if settled { 0 } else { 100 }
            );
            assert_eq!(
                ledger.spent_durable("team").unwrap(),
                if settled { 42 } else { 0 }
            );
            assert_eq!(count(&ledger), i64::from(settled));
            if settled {
                assert!(matches!(
                    ledger.settle_terminal_durable("team", &execution_id),
                    Ok(SettlementOutcome::AlreadyCommitted(_))
                ));
            }
        } else if mode.starts_with("intent_") {
            let expected = i64::from(mode == "intent_committed");
            let count: i64 = ledger
                .conn
                .query_row("SELECT COUNT(*) FROM budget_execution_intent", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(count, expected);
            assert_eq!(
                ledger.reserved_durable("team").unwrap(),
                100 + 7 * expected as u64
            );
            ledger
                .reclaim_expired_durable(OffsetDateTime::now_utc() + Duration::hours(2))
                .unwrap();
            assert_eq!(
                ledger.reserved_durable("team").unwrap(),
                7 * expected as u64
            );
        } else if mode == "committed" {
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

#[test]
fn durable_intent_survives_both_expiry_paths_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("intent.db");
    let now = OffsetDateTime::now_utc();
    let intent = {
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        ledger
            .set_limit_durable("team", Some(100), Window::Total, Policy::Block)
            .unwrap();
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_with_intent_durable("team", 100, now, Duration::seconds(1), 10)
            .unwrap()
        else {
            panic!("denied")
        };
        assert_eq!(intent.execution_id.len(), 64);
        intent
    };
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let later = now + Duration::seconds(2);
    assert_eq!(ledger.reclaim_expired_durable(later).unwrap(), 0);
    assert!(matches!(
        ledger
            .reserve_durable("team", 1, later, Duration::seconds(1))
            .unwrap(),
        ReserveOutcome::Denied(_)
    ));
    assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
    assert_eq!(
        ledger
            .intent_durable(&intent.execution_id)
            .unwrap()
            .unwrap()
            .reservation
            .id,
        intent.reservation.id
    );
    ledger
        .record_terminal_durable("team", &intent.execution_id, &observed_usage())
        .unwrap();
    let receipt = ledger
        .settle_terminal_durable("team", &intent.execution_id)
        .unwrap();
    assert!(matches!(receipt, SettlementOutcome::Committed(_)));
    assert!(matches!(
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap(),
        SettlementOutcome::AlreadyCommitted(_)
    ));
    assert_eq!(ledger.spent_durable("team").unwrap(), 42);
}

#[test]
fn intent_insert_failure_rolls_back_admission() {
    for table in ["budget_execution_intent", "budget_reservation"] {
        for failure in ["RAISE(ABORT, 'injected')", "RAISE(IGNORE)"] {
            let mut ledger = SqliteLedger::open(":memory:").unwrap();
            reserve(&mut ledger, "old");
            ledger.conn.execute_batch(&format!("CREATE TRIGGER reject_intent BEFORE INSERT ON {table} BEGIN SELECT {failure}; END;")).unwrap();
            assert!(matches!(
                ledger.reserve_with_intent_durable(
                    "team",
                    9,
                    OffsetDateTime::now_utc(),
                    Duration::seconds(2),
                    1
                ),
                Err(EvidenceError::Storage(_))
            ));
            assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
            assert_eq!(ledger.reserved_durable("old").unwrap(), 100);
            let count: i64 = ledger
                .conn
                .query_row("SELECT COUNT(*) FROM budget_execution_intent", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }
}

#[test]
fn intent_capacity_is_atomic_and_retains_settled_identities() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capacity.db");
    SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
                barrier.wait();
                ledger.reserve_with_intent_durable(
                    "team",
                    1,
                    OffsetDateTime::now_utc(),
                    Duration::seconds(1),
                    1,
                )
            })
        })
        .collect();
    let mut admitted = None;
    let mut exhausted = 0;
    for worker in workers {
        match worker.join().unwrap() {
            Ok(IntentAdmission::Admitted(intent)) => {
                assert!(admitted.is_none());
                admitted = Some(intent);
            }
            Err(EvidenceError::IntentCapacity) => exhausted += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(exhausted, 1);
    let intent = admitted.unwrap();
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    ledger
        .record_terminal_durable(
            "team",
            &intent.execution_id,
            &UsageV2 {
                tokens_in: 1,
                completeness: UsageCompleteness::Final,
                ..UsageV2::default()
            },
        )
        .unwrap();
    ledger
        .settle_terminal_durable("team", &intent.execution_id)
        .unwrap();
    assert!(matches!(
        ledger.reserve_with_intent_durable(
            "team",
            1,
            OffsetDateTime::now_utc(),
            Duration::seconds(1),
            1
        ),
        Err(EvidenceError::IntentCapacity)
    ));
    assert!(ledger
        .intent_durable(&intent.execution_id)
        .unwrap()
        .is_some());
}

#[test]
fn intent_denial_bounds_and_legacy_migration_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("intents.db");
    let path = path.to_str().unwrap();
    let mut ledger = SqliteLedger::open(path).unwrap();
    let now = OffsetDateTime::now_utc();
    for (ceiling, ttl, limit) in [
        (1, Duration::seconds(1), 0),
        (1, Duration::seconds(1), 100001),
        (u64::MAX, Duration::seconds(1), 1),
        (1, Duration::ZERO, 1),
    ] {
        assert!(matches!(
            ledger.reserve_with_intent_durable("team", ceiling, now, ttl, limit),
            Err(EvidenceError::InvalidInput)
        ));
    }
    ledger
        .set_limit_durable("team", Some(0), Window::Total, Policy::Block)
        .unwrap();
    assert!(matches!(
        ledger
            .reserve_with_intent_durable("team", 1, now, Duration::seconds(1), 1)
            .unwrap(),
        IntentAdmission::Denied(_)
    ));
    ledger
        .set_limit_durable("team", Some(i64::MAX as u64), Window::Total, Policy::Block)
        .unwrap();
    let IntentAdmission::Admitted(intent) = ledger
        .reserve_with_intent_durable("team", i64::MAX as u64, now, Duration::seconds(1), 1)
        .unwrap()
    else {
        panic!("denied")
    };
    assert!(
        matches!(
            ledger
                .reserve_durable("team", 1, now, Duration::seconds(1))
                .unwrap(),
            ReserveOutcome::Denied(_)
        ),
        "admission sum must not wrap"
    );
    assert!(ledger.intent_durable(&"0".repeat(64)).unwrap().is_none());
    assert!(matches!(
        ledger.intent_durable("invalid"),
        Err(EvidenceError::InvalidInput)
    ));
    drop(ledger);
    assert!(crate::ShardedLedger::open_sharded(path, 2).is_err());
    for shard in 0..2 {
        assert!(!std::path::Path::new(&format!("{path}-ledger-shard-{shard}.db")).exists());
    }
    let ledger = SqliteLedger::open(path).unwrap();
    assert!(ledger
        .intent_durable(&intent.execution_id)
        .unwrap()
        .is_some());
    assert_eq!(ledger.reserved_durable("team").unwrap(), i64::MAX as u64);
}

#[test]
fn terminal_settlement_binds_frozen_charge_and_blocks_caller_charge_bypasses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bound.db");
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let intent = tracked(&mut ledger);
    for phase in 0..3 {
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", intent.reservation.id, 42),
            Err(EvidenceError::TrackedSettlementRequired)
        ));
        assert!(ledger.settle_durable(intent.reservation.id, 42).is_err());
        // The legacy void trait must not change storage even though it cannot report failure.
        sandhi_core::EnforcementLedger::settle(&mut ledger, intent.reservation.id, 42);
        if phase == 0 {
            assert!(matches!(
                ledger.settle_terminal_durable("team", &intent.execution_id),
                Err(EvidenceError::MissingObservation)
            ));
            ledger
                .record_terminal_durable("team", &intent.execution_id, &observed_usage())
                .unwrap();
            // A retained snapshot may use an older version's charging formula.
            // Settlement must use its frozen value, never derive a new charge.
            ledger
                .conn
                .execute(
                    "UPDATE budget_terminal_observation SET frozen_charge = 43",
                    [],
                )
                .unwrap();
        } else if phase == 1 {
            assert!(matches!(
                ledger.settle_terminal_durable("other", &intent.execution_id),
                Err(EvidenceError::WrongScope)
            ));
            let receipt = unwrap_receipt(
                ledger
                    .settle_terminal_durable("team", &intent.execution_id)
                    .unwrap(),
            );
            assert_eq!(receipt.charged_tokens, 43);
            drop(ledger);
            ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
            assert_eq!(
                ledger
                    .settle_terminal_durable("team", &intent.execution_id)
                    .unwrap(),
                SettlementOutcome::AlreadyCommitted(receipt.clone())
            );
            let now = OffsetDateTime::now_utc();
            let claim = ledger
                .claim_settlements_durable(1, now, 60)
                .unwrap()
                .pop()
                .unwrap();
            assert!(ledger
                .acknowledge_settlement_durable(&claim.receipt.receipt_id, &claim.claim_token, now)
                .unwrap());
            assert_eq!(
                ledger
                    .settle_terminal_durable("team", &intent.execution_id)
                    .unwrap(),
                SettlementOutcome::AlreadyCommitted(receipt)
            );
        }
    }
    assert_eq!(ledger.spent_durable("team").unwrap(), 43);
    assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
    assert_eq!(count(&ledger), 1);
    assert!(matches!(
        ledger.settle_terminal_durable("team", &"0".repeat(64)),
        Err(EvidenceError::MissingIntent)
    ));
    assert!(matches!(
        ledger.settle_terminal_durable("", &intent.execution_id),
        Err(EvidenceError::InvalidInput)
    ));
    assert!(matches!(
        ledger.settle_terminal_durable("team", "bad-id"),
        Err(EvidenceError::InvalidInput)
    ));
}

#[test]
fn terminal_settlement_retains_unresolved_usage_and_accepts_measured_zero() {
    for completeness in [
        UsageCompleteness::Unavailable,
        UsageCompleteness::Partial,
        UsageCompleteness::Final,
    ] {
        for basis in [UsageBasis::ProviderReported, UsageBasis::Estimated] {
            let mut ledger = SqliteLedger::open(":memory:").unwrap();
            let intent = tracked(&mut ledger);
            let usage = UsageV2 {
                completeness,
                basis,
                ..UsageV2::default()
            };
            ledger
                .record_terminal_durable("team", &intent.execution_id, &usage)
                .unwrap();
            let resolved =
                completeness == UsageCompleteness::Final && basis == UsageBasis::ProviderReported;
            let page = ledger.recovery_page_durable("team", None, 100).unwrap();
            assert_eq!(
                page.entries[0].state,
                if resolved {
                    RecoveryState::ReadyToSettle
                } else {
                    RecoveryState::UnresolvedObservation
                }
            );
            let result = ledger.settle_terminal_durable("team", &intent.execution_id);
            if resolved {
                assert_eq!(unwrap_receipt(result.unwrap()).charged_tokens, 0);
            } else {
                assert!(matches!(result, Err(EvidenceError::UnresolvedObservation)));
            }
            ledger
                .reclaim_expired_durable(OffsetDateTime::now_utc() + Duration::hours(1))
                .unwrap();
            assert_eq!(
                ledger.reserved_durable("team").unwrap(),
                if resolved { 0 } else { 100 }
            );
            assert_eq!(ledger.spent_durable("team").unwrap(), 0);
            assert_eq!(count(&ledger), i64::from(resolved));
        }
    }
}

#[test]
fn terminal_settlement_failure_preserves_observation_and_liability() {
    for fault in ["RAISE(IGNORE)", "RAISE(ABORT, 'injected')"] {
        let mut ledger = SqliteLedger::open(":memory:").unwrap();
        let intent = tracked(&mut ledger);
        let observed = ledger
            .record_terminal_durable("team", &intent.execution_id, &observed_usage())
            .unwrap();
        ledger.conn.execute_batch(&format!("CREATE TRIGGER reject_receipt BEFORE INSERT ON budget_settlement_outbox BEGIN SELECT {fault}; END;")).unwrap();
        assert!(matches!(
            ledger.settle_terminal_durable("team", &intent.execution_id),
            Err(EvidenceError::Storage(_))
        ));
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        assert_eq!(count(&ledger), 0);
        assert_eq!(
            ledger
                .terminal_durable("team", &intent.execution_id)
                .unwrap(),
            Some(unwrap_observation(observed))
        );
        ledger
            .conn
            .execute_batch("DROP TRIGGER reject_receipt;")
            .unwrap();
        assert_eq!(
            unwrap_receipt(
                ledger
                    .settle_terminal_durable("team", &intent.execution_id)
                    .unwrap()
            )
            .charged_tokens,
            42
        );
    }
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    let intent = tracked(&mut ledger);
    ledger
        .record_terminal_durable("team", &intent.execution_id, &observed_usage())
        .unwrap();
    ledger
        .conn
        .execute(
            "UPDATE budget_terminal_observation SET usage_json = '{}'",
            [],
        )
        .unwrap();
    assert!(matches!(
        ledger.settle_terminal_durable("team", &intent.execution_id),
        Err(EvidenceError::CorruptObservation)
    ));
    assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
}

#[test]
fn terminal_settlement_concurrent_replay_returns_one_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent.db");
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let intent = tracked(&mut ledger);
    ledger
        .record_terminal_durable("team", &intent.execution_id, &observed_usage())
        .unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let mut connection = SqliteLedger::open(path.to_str().unwrap()).unwrap();
            let barrier = barrier.clone();
            let id = intent.execution_id.clone();
            std::thread::spawn(move || {
                barrier.wait();
                connection.settle_terminal_durable("team", &id).unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, SettlementOutcome::Committed(_)))
            .count(),
        1
    );
    let receipts: Vec<_> = outcomes.into_iter().map(unwrap_receipt).collect();
    assert!(receipts.iter().all(|r| r == &receipts[0]));
    assert_eq!(count(&ledger), 1);
    assert_eq!(ledger.spent_durable("team").unwrap(), 42);
    assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
}

#[test]
fn recovery_pages_are_scoped_read_only_and_bounded_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.db");
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let first = tracked(&mut ledger);
    // Interleaved untracked/other-scope rows cannot consume or escape the page.
    reserve(&mut ledger, "team");
    let foreign = tracked(&mut ledger);
    ledger
        .conn
        .execute(
            "UPDATE budget_reservation SET scope = 'other' WHERE id = ?1",
            [foreign.reservation.id],
        )
        .unwrap();
    let second = tracked(&mut ledger);
    let third = tracked(&mut ledger);
    ledger
        .record_terminal_durable("team", &second.execution_id, &observed_usage())
        .unwrap();
    ledger
        .record_terminal_durable("team", &third.execution_id, &observed_usage())
        .unwrap();
    let receipt = unwrap_receipt(
        ledger
            .settle_terminal_durable("team", &third.execution_id)
            .unwrap(),
    );
    let now = OffsetDateTime::now_utc();
    let claim = ledger
        .claim_settlements_durable(1, now, 10)
        .unwrap()
        .remove(0);
    ledger
        .acknowledge_settlement_durable(&receipt.receipt_id, &claim.claim_token, now)
        .unwrap();
    drop(ledger);
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let before = ledger.conn.total_changes();
    let reserved_before = ledger.reserved_durable("team").unwrap();
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = ledger
            .recovery_page_durable("team", cursor.as_ref(), 1)
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        entries.extend(page.entries);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        entries,
        vec![
            RecoveryEntry {
                execution_id: first.execution_id,
                reservation_id: first.reservation.id,
                state: RecoveryState::MissingObservation
            },
            RecoveryEntry {
                execution_id: second.execution_id,
                reservation_id: second.reservation.id,
                state: RecoveryState::ReadyToSettle
            },
            RecoveryEntry {
                execution_id: third.execution_id,
                reservation_id: third.reservation.id,
                state: RecoveryState::Settled(receipt)
            },
        ]
    );
    let empty = ledger.recovery_page_durable("empty", None, 100).unwrap();
    assert!(empty.entries.is_empty() && empty.next.is_none());
    assert_eq!(
        ledger
            .recovery_page_durable("other", None, 100)
            .unwrap()
            .entries[0]
            .execution_id,
        foreign.execution_id
    );
    assert_eq!(ledger.conn.total_changes(), before);
    assert_eq!(ledger.reserved_durable("team").unwrap(), reserved_before);
    assert_eq!(ledger.spent_durable("team").unwrap(), 42);
    assert_eq!(count(&ledger), 1);
}

#[test]
fn recovery_cursor_freezes_admission_range_but_rechecks_current_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recovery.db");
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let first = tracked(&mut ledger);
    let second = tracked(&mut ledger);
    let page = ledger.recovery_page_durable("team", None, 1).unwrap();
    let cursor = page.next.unwrap();
    let mut writer = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let later = tracked(&mut writer);
    for intent in [&first, &second] {
        writer
            .record_terminal_durable("team", &intent.execution_id, &observed_usage())
            .unwrap();
    }
    let next = ledger
        .recovery_page_durable("team", Some(&cursor), 100)
        .unwrap();
    assert_eq!(
        next.entries,
        vec![RecoveryEntry {
            execution_id: second.execution_id.clone(),
            reservation_id: second.reservation.id,
            state: RecoveryState::ReadyToSettle
        }]
    );
    assert!(next.next.is_none());
    // Inventory is not a claim: another connection settles before this consumer does.
    let receipt = unwrap_receipt(
        writer
            .settle_terminal_durable("team", &second.execution_id)
            .unwrap(),
    );
    assert_eq!(
        ledger
            .settle_terminal_durable("team", &second.execution_id)
            .unwrap(),
        SettlementOutcome::AlreadyCommitted(receipt)
    );
    let sweep = ledger.recovery_page_durable("team", None, 100).unwrap();
    assert_eq!(sweep.entries.len(), 3);
    assert_eq!(sweep.entries[0].state, RecoveryState::ReadyToSettle);
    assert_eq!(sweep.entries[2].execution_id, later.execution_id);
    for limit in [0, 101, usize::MAX] {
        assert!(matches!(
            ledger.recovery_page_durable("team", None, limit),
            Err(EvidenceError::InvalidInput)
        ));
    }
    assert!(matches!(
        ledger.recovery_page_durable("", None, 1),
        Err(EvidenceError::InvalidInput)
    ));
    assert!(matches!(
        ledger.recovery_page_durable(&"x".repeat(4097), None, 1),
        Err(EvidenceError::InvalidInput)
    ));
    assert!(matches!(
        ledger.recovery_page_durable("other", Some(&cursor), 1),
        Err(EvidenceError::WrongScope)
    ));
    for (after_id, through_id) in [(0, 2), (3, 2), (1, u64::MAX)] {
        let bad = RecoveryCursor {
            scope: "team".into(),
            after_id,
            through_id,
        };
        assert!(matches!(
            ledger.recovery_page_durable("team", Some(&bad), 1),
            Err(EvidenceError::InvalidInput)
        ));
    }
    let exhausted = RecoveryCursor {
        scope: "team".into(),
        after_id: second.reservation.id,
        through_id: second.reservation.id,
    };
    let empty = ledger
        .recovery_page_durable("team", Some(&exhausted), 1)
        .unwrap();
    assert!(empty.entries.is_empty() && empty.next.is_none());
}

#[test]
fn recovery_rejects_damaged_bindings_observations_and_receipts_without_writes() {
    for fault in [
        "DELETE FROM budget_reservation",
        "PRAGMA foreign_keys = OFF; DELETE FROM budget_execution_intent",
        "DELETE FROM budget_terminal_observation",
        "DELETE FROM budget_settlement_outbox",
        "UPDATE budget_terminal_observation SET usage_json = '{}'",
        "UPDATE budget_terminal_observation SET frozen_charge = 43",
        "UPDATE budget_reservation SET settled = 0",
        "UPDATE budget_reservation SET settled = 2",
        "UPDATE budget_reservation SET actual = 43",
        "UPDATE budget_reservation SET settled_at = 0",
        "UPDATE budget_settlement_outbox SET scope = 'other'",
        "UPDATE budget_settlement_outbox SET charged_tokens = 43",
        "UPDATE budget_settlement_outbox SET receipt_id = 'invalid'",
        "UPDATE budget_settlement_outbox SET receipt_id = printf('%20000s', 'x')",
        "UPDATE budget_settlement_outbox SET scope = printf('%20000s', 'x')",
    ] {
        let mut ledger = SqliteLedger::open(":memory:").unwrap();
        let intent = tracked(&mut ledger);
        ledger
            .record_terminal_durable("team", &intent.execution_id, &observed_usage())
            .unwrap();
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        ledger.conn.execute_batch(fault).unwrap();
        let before = ledger.conn.total_changes();
        assert!(
            ledger.recovery_page_durable("team", None, 1).is_err(),
            "{fault}"
        );
        assert_eq!(ledger.conn.total_changes(), before, "{fault}");
        assert!(
            ledger
                .settle_terminal_durable("team", &intent.execution_id)
                .is_err(),
            "{fault}"
        );
        assert_eq!(ledger.conn.total_changes(), before, "{fault}");
    }
    // Malformed identity allocation is bounded before decoding.
    for invalid in ["g".repeat(64), "x".repeat(20000)] {
        let mut ledger = SqliteLedger::open(":memory:").unwrap();
        tracked(&mut ledger);
        ledger
            .conn
            .execute(
                "UPDATE budget_execution_intent SET execution_id = ?1",
                [invalid],
            )
            .unwrap();
        assert!(matches!(
            ledger.recovery_page_durable("team", None, 1),
            Err(EvidenceError::CorruptObservation)
        ));
    }
    // Even a request for a different scope cannot silently certify an orphaned intent.
    let mut ledger = SqliteLedger::open(":memory:").unwrap();
    tracked(&mut ledger);
    ledger
        .conn
        .execute("DELETE FROM budget_reservation", [])
        .unwrap();
    assert!(matches!(
        ledger.recovery_page_durable("other", None, 1),
        Err(EvidenceError::CorruptObservation)
    ));
}

// These cases extend the existing evidence suite: legacy/terminal/receipt matrices
// remain above. This boundary owns only the opt-in dispatch/closure transition.
#[test]
fn prepared_closure_is_durable_without_provider_usage_or_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let path = path.to_str().unwrap();
    let mut ledger = SqliteLedger::open(path).unwrap();
    for action in ["IGNORE", "ABORT, 'injected failure'"] {
        ledger.conn.execute_batch(&format!("CREATE TRIGGER fail_fence BEFORE INSERT ON budget_dispatch_fence BEGIN SELECT RAISE({action}); END;")).unwrap();
        assert!(matches!(
            ledger.reserve_prepared_durable(
                "team",
                100,
                OffsetDateTime::now_utc(),
                Duration::seconds(30),
                10
            ),
            Err(EvidenceError::Storage(_))
        ));
        for table in [
            "budget_reservation",
            "budget_execution_intent",
            "budget_dispatch_fence",
        ] {
            assert_eq!(
                ledger
                    .conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        ledger
            .conn
            .execute_batch("DROP TRIGGER fail_fence")
            .unwrap();
    }
    let IntentAdmission::Admitted(intent) = ledger
        .reserve_prepared_durable(
            "team",
            100,
            OffsetDateTime::now_utc(),
            Duration::seconds(30),
            10,
        )
        .unwrap()
    else {
        panic!("denied")
    };
    assert!(matches!(
        ledger
            .recovery_page_durable("team", None, 10)
            .unwrap()
            .entries[0]
            .state,
        RecoveryState::Prepared
    ));
    assert!(matches!(
        ledger.record_terminal_durable("team", &intent.execution_id, &observed_usage()),
        Err(EvidenceError::DispatchNotAuthorized)
    ));
    assert!(matches!(
        ledger.close_before_dispatch_durable("other", &intent.execution_id),
        Err(EvidenceError::WrongScope)
    ));
    ledger.conn.execute_batch("CREATE TRIGGER fail_closure BEFORE UPDATE ON budget_reservation BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert!(matches!(
        ledger.close_before_dispatch_durable("team", &intent.execution_id),
        Err(EvidenceError::Storage(_))
    ));
    assert!(matches!(
        ledger
            .recovery_page_durable("team", None, 10)
            .unwrap()
            .entries[0]
            .state,
        RecoveryState::Prepared
    ));
    ledger
        .conn
        .execute_batch("DROP TRIGGER fail_closure")
        .unwrap();
    let ClosureOutcome::Closed(closure) = ledger
        .close_before_dispatch_durable("team", &intent.execution_id)
        .unwrap()
    else {
        panic!("not closed")
    };
    drop(ledger);
    let mut ledger = SqliteLedger::open(path).unwrap();
    assert_eq!(
        ledger
            .close_before_dispatch_durable("team", &intent.execution_id)
            .unwrap(),
        ClosureOutcome::AlreadyClosed(closure.clone())
    );
    assert!(
        matches!(ledger.authorize_dispatch_durable("team", &intent.execution_id).unwrap(), DispatchOutcome::ClosedBeforeDispatch(value) if value == closure)
    );
    assert_eq!(
        ledger
            .recovery_page_durable("team", None, 10)
            .unwrap()
            .entries[0]
            .state,
        RecoveryState::ClosedBeforeDispatch(closure)
    );
    assert!(matches!(
        ledger.record_terminal_durable("team", &intent.execution_id, &observed_usage()),
        Err(EvidenceError::DispatchNotAuthorized)
    ));
    assert!(matches!(
        ledger.settle_terminal_durable("team", &intent.execution_id),
        Err(EvidenceError::DispatchNotAuthorized)
    ));
    assert!(ledger
        .terminal_durable("team", &intent.execution_id)
        .unwrap()
        .is_none());
    assert!(ledger
        .claim_settlements_durable(10, OffsetDateTime::now_utc(), 30)
        .unwrap()
        .is_empty());
    assert_eq!(ledger.conn.query_row("SELECT SUM(actual), SUM(CASE WHEN settled=0 THEN ceiling ELSE 0 END) FROM budget_reservation", [], |r| Ok((r.get::<_,u64>(0)?,r.get::<_,u64>(1)?))).unwrap(), (0,0));
}

#[test]
fn dispatch_authorization_survives_lost_permit_and_retains_liability() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let path = path.to_str().unwrap();
    let mut ledger = SqliteLedger::open(path).unwrap();
    let IntentAdmission::Admitted(intent) = ledger
        .reserve_prepared_durable(
            "team",
            100,
            OffsetDateTime::now_utc(),
            Duration::seconds(30),
            10,
        )
        .unwrap()
    else {
        panic!("denied")
    };
    let DispatchOutcome::Authorized(permit) = ledger
        .authorize_dispatch_durable("team", &intent.execution_id)
        .unwrap()
    else {
        panic!("no permit")
    };
    assert_eq!(permit.execution_id(), intent.execution_id);
    drop(permit);
    drop(ledger);
    let mut ledger = SqliteLedger::open(path).unwrap();
    assert!(matches!(
        ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap(),
        DispatchOutcome::MayHaveDispatched
    ));
    assert_eq!(
        ledger
            .close_before_dispatch_durable("team", &intent.execution_id)
            .unwrap(),
        ClosureOutcome::MayHaveDispatched
    );
    assert_eq!(
        ledger
            .recovery_page_durable("team", None, 10)
            .unwrap()
            .entries[0]
            .state,
        RecoveryState::MayHaveDispatched
    );
    assert_eq!(
        ledger
            .conn
            .query_row(
                "SELECT ceiling FROM budget_reservation WHERE settled=0",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        100
    );
    ledger
        .record_terminal_durable("team", &intent.execution_id, &observed_usage())
        .unwrap();
    assert!(matches!(
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap(),
        SettlementOutcome::Committed(_)
    ));
    assert_eq!(
        ledger
            .close_before_dispatch_durable("team", &intent.execution_id)
            .unwrap(),
        ClosureOutcome::MayHaveDispatched
    );
}

#[test]
fn legacy_intents_never_acquire_never_dispatched_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let path = path.to_str().unwrap();
    let mut ledger = SqliteLedger::open(path).unwrap();
    let old = tracked(&mut ledger);
    // Simulate the pre-fence schema, then reopen through the additive migration.
    ledger
        .conn
        .execute_batch("DROP TABLE budget_dispatch_fence")
        .unwrap();
    drop(ledger);
    let mut ledger = SqliteLedger::open(path).unwrap();
    let new = tracked(&mut ledger);
    for intent in [old, new] {
        assert!(matches!(
            ledger.authorize_dispatch_durable("team", &intent.execution_id),
            Err(EvidenceError::UnfencedIntent)
        ));
        assert!(matches!(
            ledger.close_before_dispatch_durable("team", &intent.execution_id),
            Err(EvidenceError::UnfencedIntent)
        ));
        ledger
            .record_terminal_durable("team", &intent.execution_id, &observed_usage())
            .unwrap();
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
    }
    let mut memory = SqliteLedger::open(":memory:").unwrap();
    assert!(matches!(
        memory.reserve_prepared_durable(
            "team",
            100,
            OffsetDateTime::now_utc(),
            Duration::seconds(30),
            10
        ),
        Err(EvidenceError::UnsupportedTrackedLedger)
    ));
}

#[test]
fn dispatch_and_closure_serialize_across_connections_for_both_winners() {
    static BUSY_READY: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>> =
        std::sync::Mutex::new(None);
    for dispatch_wins in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let path = path.to_str().unwrap().to_string();
        let mut first = SqliteLedger::open(&path).unwrap();
        let mut second = SqliteLedger::open(&path).unwrap();
        let IntentAdmission::Admitted(intent) = first
            .reserve_prepared_durable(
                "team",
                100,
                OffsetDateTime::now_utc(),
                Duration::seconds(30),
                10,
            )
            .unwrap()
        else {
            panic!("denied")
        };
        // Hold the writer lock before starting the competing connection. The loser
        // must reread the committed winner, never act on a pre-transaction read.
        let tx = first
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        *BUSY_READY.lock().unwrap() = Some(ready_tx);
        second
            .conn
            .busy_handler(Some(|_| {
                if let Some(sender) = BUSY_READY.lock().unwrap().take() {
                    sender.send(()).unwrap();
                }
                std::thread::yield_now();
                true
            }))
            .unwrap();
        let execution_id = intent.execution_id.clone();
        let worker = std::thread::spawn(move || {
            if dispatch_wins {
                assert_eq!(
                    second
                        .close_before_dispatch_durable("team", &execution_id)
                        .unwrap(),
                    ClosureOutcome::MayHaveDispatched
                );
            } else {
                assert!(matches!(
                    second
                        .authorize_dispatch_durable("team", &execution_id)
                        .unwrap(),
                    DispatchOutcome::ClosedBeforeDispatch(_)
                ));
            }
        });
        // The callback fires only once SQLite actually encounters the held writer lock.
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (_, changed) =
            dispatch::transition(&tx, "team", &intent.execution_id, dispatch_wins).unwrap();
        assert!(changed);
        tx.commit().unwrap();
        worker.join().unwrap();
    }
}

#[test]
fn dispatch_fence_rejects_expired_and_damaged_evidence() {
    for damage in [
        "expiry",
        "orphan",
        "timestamp",
        "phase",
        "prepared_receipt",
        "closed_observation",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_prepared_durable(
                "team",
                100,
                OffsetDateTime::now_utc(),
                Duration::seconds(30),
                10,
            )
            .unwrap()
        else {
            panic!("denied")
        };
        match damage {
            "expiry" => {
                ledger
                    .conn
                    .execute_batch("UPDATE budget_reservation SET expires_at=0")
                    .unwrap();
                assert!(matches!(
                    ledger.authorize_dispatch_durable("team", &intent.execution_id),
                    Err(EvidenceError::ExpiredBeforeDispatch)
                ));
                assert!(matches!(
                    ledger
                        .close_before_dispatch_durable("team", &intent.execution_id)
                        .unwrap(),
                    ClosureOutcome::Closed(_)
                ));
                continue;
            }
            "orphan" => {
                ledger
                    .conn
                    .execute_batch("DELETE FROM budget_reservation")
                    .unwrap();
            }
            "timestamp" => {
                ledger.conn.execute_batch("UPDATE budget_dispatch_fence SET phase=1, transitioned_at=9223372036854775807").unwrap();
            }
            "phase" => {
                ledger.conn.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE budget_dispatch_fence SET phase=9").unwrap();
            }
            "prepared_receipt" => {
                ledger.conn.execute_batch("INSERT INTO budget_settlement_outbox(receipt_id,reservation_id,scope,charged_tokens,settled_at) SELECT 'fake',id,scope,0,0 FROM budget_reservation").unwrap();
            }
            "closed_observation" => {
                ledger
                    .close_before_dispatch_durable("team", &intent.execution_id)
                    .unwrap();
                ledger.conn.execute("INSERT INTO budget_terminal_observation(execution_id,version,usage_json,frozen_charge,observed_at) VALUES (?1,1,?2,37,0)",params![intent.execution_id,encode_usage(&observed_usage()).unwrap()]).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .is_err(),
            "{damage}"
        );
        assert!(
            ledger
                .close_before_dispatch_durable("team", &intent.execution_id)
                .is_err(),
            "{damage}"
        );
        assert!(
            ledger.recovery_page_durable("team", None, 10).is_err(),
            "{damage}"
        );
    }
}
