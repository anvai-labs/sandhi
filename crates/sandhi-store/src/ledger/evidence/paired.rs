//! Two reservations, one execution/observation/receipt, on the original ledger.
use super::*;

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS budget_execution_back_leg (
        execution_id TEXT PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        version INTEGER NOT NULL CHECK(version=1),
        reservation_id INTEGER UNIQUE NOT NULL REFERENCES budget_reservation(id) CHECK(reservation_id>0)
    );")?;
    let exists: bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('budget_dispatch_fence') WHERE name='back_reservation_id')",[],|r|r.get(0))?;
    if !exists {
        conn.execute("ALTER TABLE budget_dispatch_fence ADD COLUMN back_reservation_id INTEGER CHECK(back_reservation_id>0)",[])?;
    }
    conn.execute("CREATE UNIQUE INDEX IF NOT EXISTS idx_dispatch_back_reservation ON budget_dispatch_fence(back_reservation_id) WHERE back_reservation_id IS NOT NULL",[])?;
    Ok(())
}

#[derive(PartialEq, Eq)]
struct State {
    actual: i64,
    settled: i64,
    at: Option<i64>,
}
struct StoredReservation {
    scope: Option<String>,
    ceiling: i64,
    expires: i64,
    state: State,
}
fn reservation(conn: &Connection, id: u64) -> Result<(Reservation, State), EvidenceError> {
    let row: Option<StoredReservation>=conn.query_row(
        "SELECT CASE WHEN typeof(scope)='text' AND length(CAST(scope AS BLOB))<=4096 THEN scope END,
         ceiling,expires_at,actual,settled,settled_at FROM budget_reservation WHERE id=?1",
        [id as i64],|r|Ok(StoredReservation {scope:r.get(0)?,ceiling:r.get(1)?,expires:r.get(2)?,state:State{actual:r.get(3)?,settled:r.get(4)?,at:r.get(5)?}})).optional()?;
    let StoredReservation {
        scope,
        ceiling,
        expires,
        state: State {
            actual,
            settled,
            at,
        },
    } = row.ok_or(EvidenceError::MissingReservation)?;
    let scope = scope.ok_or(EvidenceError::CorruptObservation)?;
    validate_scope(&scope)?;
    if id == 0
        || id > i64::MAX as u64
        || ceiling < 0
        || actual < 0
        || !matches!((settled, at), (0, None) | (1, Some(_)))
        || (settled == 0 && actual != 0)
        || at.is_some_and(|at| OffsetDateTime::from_unix_timestamp(at).is_err())
    {
        return Err(EvidenceError::InconsistentSettlement);
    }
    Ok((
        Reservation {
            id,
            scope,
            ceiling: ceiling as u64,
            expires_at: OffsetDateTime::from_unix_timestamp(expires)
                .map_err(|_| EvidenceError::CorruptObservation)?,
        },
        State {
            actual,
            settled,
            at,
        },
    ))
}

/// Validate required linkage and both reservation states in the caller's transaction.
/// A linked back leg has no independent receipt or caller-selected charge.
pub(super) fn read(
    conn: &Connection,
    execution_id: &str,
    primary_id: u64,
) -> Result<Option<Reservation>, EvidenceError> {
    let row: Option<(Option<i64>, Option<i64>, Option<i64>)> = conn
        .query_row(
            "SELECT f.back_reservation_id,b.version,b.reservation_id FROM budget_execution_intent i
         LEFT JOIN budget_dispatch_fence f USING(execution_id)
         LEFT JOIN budget_execution_back_leg b USING(execution_id) WHERE i.execution_id=?1",
            [execution_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let id = match row {
        None | Some((None, None, None)) => return Ok(None),
        Some((Some(required), Some(1), Some(id)))
            if required == id && id > 0 && id as u64 != primary_id =>
        {
            id as u64
        }
        _ => return Err(EvidenceError::CorruptObservation),
    };
    let context = context::read(conn, execution_id)?.ok_or(EvidenceError::CorruptObservation)?;
    let (primary, primary_state) = reservation(conn, primary_id)?;
    let (back, back_state) = reservation(conn, id)?;
    let other_claim: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM budget_execution_intent WHERE reservation_id=?1)
        OR EXISTS(SELECT 1 FROM budget_settlement_outbox WHERE reservation_id=?1)",
        [id as i64],
        |r| r.get(0),
    )?;
    if other_claim
        || back.scope != context.upstream_budget_scope
        || back.scope == primary.scope
        || back.ceiling != primary.ceiling
        || back.expires_at != primary.expires_at
        || back_state != primary_state
    {
        return Err(EvidenceError::InconsistentSettlement);
    }
    Ok(Some(back))
}

pub(super) fn close(
    tx: &rusqlite::Transaction<'_>,
    back: Option<&Reservation>,
    at: i64,
) -> Result<(), EvidenceError> {
    if let Some(back) = back {
        let changed=tx.execute("UPDATE budget_reservation SET settled=1,actual=0,settled_at=?2 WHERE id=?1 AND settled=0",params![back.id as i64,at])?;
        if changed != 1 {
            return Err(EvidenceError::InconsistentSettlement);
        }
    }
    Ok(())
}
pub(super) fn settle(
    tx: &rusqlite::Transaction<'_>,
    back: Option<&Reservation>,
    outcome: &SettlementOutcome,
) -> Result<(), EvidenceError> {
    if let Some(back) = back {
        let receipt = match outcome {
            SettlementOutcome::Committed(r) | SettlementOutcome::AlreadyCommitted(r) => r,
        };
        tx.execute("UPDATE budget_reservation SET settled=1,actual=?2,settled_at=?3 WHERE id=?1 AND settled=0",
            params![back.id as i64,receipt.charged_tokens as i64,receipt.settled_at])?;
    }
    Ok(())
}

impl SqliteLedger {
    /// Scope-checked evidence lookup; this ID grants no inference/replay authority.
    pub fn execution_back_leg_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<Reservation>, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let (id, stored_scope, _) =
            intent_binding(&tx, execution_id)?.ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        let back = read(&tx, execution_id, id)?;
        tx.commit()?;
        Ok(back)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> context::ExecutionContext {
        serde_json::from_value(serde_json::json!({"version":1,"ingress":"openai_chat_completions","delivery":"buffered","plane":"transparent",
            "upstream_ref":"openai:default","upstream_budget_scope":"upstream:models","requested_model":"model","actual_model":"model","provider":"models",
            "endpoint_family":"openai_chat_completions","transport_binding_sha256":"a".repeat(64),"accounting_policy":{"version":1,"cache_read_weight":1000}})).unwrap()
    }
    fn admit(ledger: &mut SqliteLedger) -> ExecutionIntent {
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_paired_contextual_durable(
                "team",
                100,
                OffsetDateTime::now_utc(),
                Duration::seconds(60),
                100,
                "request",
                &context(),
            )
            .unwrap()
        else {
            panic!("admission")
        };
        intent
    }
    fn usage() -> UsageV2 {
        UsageV2 {
            tokens_in: 3,
            tokens_out: 2,
            cache_read_tokens: 10,
            completeness: UsageCompleteness::Final,
            ..Default::default()
        }
    }

    #[test]
    fn admission_progress_locates_a_real_writer_wait_without_early_reservations() {
        use std::sync::{mpsc, Arc};
        use std::time::{Duration as WallDuration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admission-progress.db");
        let ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        let progress = Arc::new(AdmissionProgress::new(Instant::now()));
        let observed = progress.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut ledger = ledger;
            let result = ledger.reserve_paired_contextual_durable_observed(
                "team",
                100,
                OffsetDateTime::now_utc(),
                Duration::seconds(60),
                100,
                "blocked-admission",
                &context(),
                Some(&observed),
            );
            result_tx.send(result).unwrap();
            ledger
        });
        let until = Instant::now() + WallDuration::from_secs(2);
        while progress.snapshot().stage != "begin_transaction" && Instant::now() < until {
            std::thread::yield_now();
        }
        let at_wait = progress.snapshot();
        let pending = matches!(
            result_rx.recv_timeout(WallDuration::from_millis(10)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        let held = connection
            .query_row("SELECT count(*) FROM budget_reservation", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        // Release the controlled writer before assertions can unwind.
        connection.execute_batch("ROLLBACK").unwrap();
        let result = result_rx
            .recv_timeout(WallDuration::from_secs(2))
            .unwrap()
            .unwrap();
        let ledger = worker.join().unwrap();
        assert_eq!(at_wait.stage, "begin_transaction");
        assert!(at_wait.stage_us.is_some());
        assert!(pending);
        assert_eq!(held, 0);
        assert_eq!(progress.snapshot().stage, "returned");
        let IntentAdmission::Admitted(intent) = result else {
            panic!("admission");
        };
        assert_eq!(
            ledger
                .request_id_durable("team", &intent.execution_id)
                .unwrap()
                .as_deref(),
            Some("blocked-admission")
        );
        assert_eq!(
            ledger
                .conn
                .query_row("SELECT count(*) FROM budget_reservation", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            ledger
                .conn
                .query_row("SELECT count(*) FROM budget_settlement_outbox", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            ledger
                .conn
                .query_row(
                    "SELECT phase FROM budget_dispatch_fence WHERE execution_id=?1",
                    [&intent.execution_id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        progress.reset();
        let snapshot = progress.snapshot();
        assert_eq!(snapshot.stage, "unobserved");
        assert_eq!(snapshot.stage_us, None);
    }

    #[test]
    fn observed_admission_keeps_context_failure_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admission-progress.db");
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        ledger.conn.execute_batch("CREATE TRIGGER fail_context BEFORE INSERT ON budget_execution_context BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        let progress = AdmissionProgress::new(std::time::Instant::now());
        assert!(ledger
            .reserve_paired_contextual_durable_observed(
                "team",
                100,
                OffsetDateTime::now_utc(),
                Duration::seconds(60),
                100,
                "failed-admission",
                &context(),
                Some(&progress),
            )
            .is_err());
        assert_eq!(progress.snapshot().stage, "write_context");
        for table in [
            "budget_reservation",
            "budget_execution_intent",
            "budget_dispatch_fence",
            "budget_execution_context",
            "budget_execution_back_leg",
        ] {
            assert_eq!(
                ledger
                    .conn
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
    fn evidence(ledger: &SqliteLedger, intent: &ExecutionIntent) -> AttemptEvidence {
        AttemptEvidence {
            observed_provider: None,
            version: 1,
            attempt_id: ledger
                .physical_attempt_durable("team", &intent.execution_id)
                .unwrap()
                .unwrap()
                .attempt_id,
            attempt_ordinal: 1,
            dispatch_observed_at_ms: Some(1000),
            terminal: Some(TransportTerminal {
                observed_at_ms: 999, // wall clocks can move backwards
                outcome: TransportOutcome::Success,
                provider_status: Some(200),
                provider_request_id: Some("fixture-request".into()),
            }),
            dropped_observations: 0,
            invalid: false,
        }
    }
    #[test]
    fn paired_admission_denial_and_link_failure_roll_back_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        ledger
            .set_limit_durable("upstream:models", Some(50), Window::Total, Policy::Block)
            .unwrap();
        assert!(matches!(
            ledger
                .reserve_paired_contextual_durable(
                    "team",
                    100,
                    OffsetDateTime::now_utc(),
                    Duration::seconds(60),
                    100,
                    "request",
                    &context()
                )
                .unwrap(),
            IntentAdmission::Denied(_)
        ));
        ledger
            .set_limit_durable("upstream:models", Some(1000), Window::Total, Policy::Block)
            .unwrap();
        ledger.conn.execute_batch("CREATE TRIGGER fail_link BEFORE INSERT ON budget_execution_back_leg BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        assert!(ledger
            .reserve_paired_contextual_durable(
                "team",
                100,
                OffsetDateTime::now_utc(),
                Duration::seconds(60),
                100,
                "request",
                &context()
            )
            .is_err());
        for table in [
            "budget_reservation",
            "budget_execution_intent",
            "budget_dispatch_fence",
            "budget_request_correlation",
            "budget_execution_context",
            "budget_execution_back_leg",
        ] {
            assert_eq!(
                ledger
                    .conn
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
    #[test]
    fn paired_unknown_usage_survives_expiry_restart_and_legacy_release_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let path = path.to_str().unwrap();
        let mut ledger = SqliteLedger::open(path).unwrap();
        let intent = admit(&mut ledger);
        let back = ledger
            .execution_back_leg_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            DispatchOutcome::Authorized(_)
        ));
        let evidence = evidence(&ledger, &intent);
        ledger
            .record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &UsageV2::default(),
                1000,
                &evidence,
            )
            .unwrap();
        assert!(matches!(
            ledger.settle_terminal_durable("team", &intent.execution_id),
            Err(EvidenceError::UnresolvedObservation)
        ));
        assert!(ledger.settle_durable(back.id, 0).is_err());
        assert!(matches!(
            ledger.settle_with_evidence_durable(&back.scope, back.id, 0),
            Err(EvidenceError::TrackedSettlementRequired)
        ));
        drop(ledger);
        let mut ledger = SqliteLedger::open(path).unwrap();
        assert_eq!(
            ledger
                .reclaim_expired_durable(OffsetDateTime::now_utc() + Duration::hours(1))
                .unwrap(),
            0
        );
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(ledger.reserved_durable("upstream:models").unwrap(), 100);
        // The reserve-time reclaim path must preserve the same unknown pair as
        // the explicit sweep; otherwise a later admission could erase liability.
        for scope in ["team", "upstream:models"] {
            ledger
                .set_limit_durable(scope, Some(100), Window::Total, Policy::Block)
                .unwrap();
            assert!(matches!(
                ledger
                    .reserve_durable(
                        scope,
                        1,
                        OffsetDateTime::now_utc() + Duration::hours(1),
                        Duration::seconds(60)
                    )
                    .unwrap(),
                ReserveOutcome::Denied(_)
            ));
            assert_eq!(ledger.reserved_durable(scope).unwrap(), 100);
        }
        assert!(matches!(
            ledger
                .recovery_page_durable("team", None, 10)
                .unwrap()
                .entries[0]
                .state,
            RecoveryState::UnresolvedObservation
        ));
    }
    #[test]
    fn paired_settlement_is_atomic_and_recovers_one_original_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let path = path.to_str().unwrap();
        let mut ledger = SqliteLedger::open(path).unwrap();
        let intent = admit(&mut ledger);
        let back = ledger
            .execution_back_leg_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap();
        ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap();
        let evidence = evidence(&ledger, &intent);
        ledger
            .record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &usage(),
                1000,
                &evidence,
            )
            .unwrap();
        ledger.conn.execute_batch(&format!("CREATE TRIGGER fail_back_settle BEFORE UPDATE OF settled ON budget_reservation WHEN OLD.id={} BEGIN SELECT RAISE(ABORT,'fixture'); END",back.id)).unwrap();
        assert!(ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .is_err());
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        assert_eq!(ledger.spent_durable(&back.scope).unwrap(), 0);
        assert_eq!(
            ledger
                .conn
                .query_row("SELECT count(*) FROM budget_settlement_outbox", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(ledger.reserved_durable(&back.scope).unwrap(), 100);
        ledger
            .conn
            .execute_batch("DROP TRIGGER fail_back_settle")
            .unwrap();
        drop(ledger);
        let mut ledger = SqliteLedger::open(path).unwrap();
        let outcome = ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        assert!(
            matches!(&outcome,SettlementOutcome::Committed(receipt) if receipt.charged_tokens==6)
        );
        assert_eq!(ledger.spent_durable("team").unwrap(), 6);
        assert_eq!(ledger.spent_durable(&back.scope).unwrap(), 6);
        let SettlementOutcome::AlreadyCommitted(replayed) = ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap()
        else {
            panic!("receipt replay")
        };
        let SettlementOutcome::Committed(original) = outcome else {
            panic!("first receipt")
        };
        assert_eq!(replayed, original);
        assert_eq!(
            ledger
                .execution_back_leg_durable("team", &intent.execution_id)
                .unwrap()
                .unwrap()
                .id,
            back.id
        );
    }
    #[test]
    fn paired_closure_releases_both_only_before_dispatch_and_corruption_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        let intent = admit(&mut ledger);
        let back = ledger
            .execution_back_leg_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap();
        assert!(matches!(
            ledger
                .close_before_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            ClosureOutcome::Closed(_)
        ));
        assert!(ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .is_none());
        assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
        assert_eq!(ledger.reserved_durable(&back.scope).unwrap(), 0);
        assert!(matches!(
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            DispatchOutcome::ClosedBeforeDispatch(_)
        ));
        let intent = admit(&mut ledger);
        let back = ledger
            .execution_back_leg_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap();
        ledger
            .conn
            .execute(
                "DELETE FROM budget_execution_back_leg WHERE execution_id=?1",
                [&intent.execution_id],
            )
            .unwrap();
        assert!(ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .is_err());
        assert!(ledger.recovery_page_durable("team", None, 10).is_err());
        assert_eq!(
            ledger
                .reclaim_expired_durable(OffsetDateTime::now_utc() + Duration::hours(1))
                .unwrap(),
            0
        );
        assert!(
            ledger.settle_durable(back.id, 0).is_err(),
            "lost link cannot authorize a legacy zero release"
        );
    }

    #[test]
    fn attempt_allocation_rolls_back_with_authorization_and_replay_never_gets_a_permit() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger =
            SqliteLedger::open(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        let intent = admit(&mut ledger);
        assert!(ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .is_none());
        ledger.conn.execute_batch("CREATE TRIGGER fail_attempt BEFORE INSERT ON budget_physical_attempt BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        assert!(ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .is_err());
        assert_eq!(
            ledger
                .recovery_page_durable("team", None, 10)
                .unwrap()
                .entries[0]
                .state,
            RecoveryState::Prepared
        );
        ledger
            .conn
            .execute_batch("DROP TRIGGER fail_attempt")
            .unwrap();
        let DispatchOutcome::Authorized(permit) = ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap()
        else {
            panic!("first permit")
        };
        let attempt = ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap();
        assert_eq!(permit.attempt_id(), Some(attempt.attempt_id.as_str()));
        assert!(attempt.evidence.is_none());
        assert!(matches!(
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            DispatchOutcome::MayHaveDispatched
        ));
        assert_eq!(
            ledger
                .physical_attempt_durable("team", &intent.execution_id)
                .unwrap(),
            Some(attempt)
        );
        assert!(matches!(
            ledger.physical_attempt_durable("other", &intent.execution_id),
            Err(EvidenceError::WrongScope)
        ));
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &usage(), 1000),
            Err(EvidenceError::ConflictingObservation)
        ));
    }

    #[test]
    fn attempt_evidence_and_usage_commit_together_and_never_accept_refinements() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger =
            SqliteLedger::open(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        let intent = admit(&mut ledger);
        ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap();
        let evidence = evidence(&ledger, &intent);
        ledger.conn.execute_batch("CREATE TRIGGER fail_terminal BEFORE INSERT ON budget_terminal_observation BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        assert!(ledger
            .record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &usage(),
                1000,
                &evidence
            )
            .is_err());
        assert!(ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap()
            .evidence
            .is_none());
        assert!(ledger
            .terminal_durable("team", &intent.execution_id)
            .unwrap()
            .is_none());
        ledger
            .conn
            .execute_batch("DROP TRIGGER fail_terminal")
            .unwrap();
        let first = ledger
            .record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &usage(),
                1000,
                &evidence,
            )
            .unwrap();
        let ObservationOutcome::Recorded(first) = first else {
            panic!("new observation")
        };
        assert_eq!(
            ledger
                .record_terminal_with_attempt_durable(
                    "team",
                    &intent.execution_id,
                    &usage(),
                    1000,
                    &evidence
                )
                .unwrap(),
            ObservationOutcome::AlreadyRecorded(first)
        );
        let mut changed = evidence.clone();
        changed.terminal.as_mut().unwrap().provider_request_id = Some("different".into());
        assert!(matches!(
            ledger.record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &usage(),
                1000,
                &changed
            ),
            Err(EvidenceError::ConflictingObservation)
        ));
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        assert_eq!(
            ledger
                .physical_attempt_durable("team", &intent.execution_id)
                .unwrap()
                .unwrap()
                .evidence,
            Some(evidence)
        );
    }

    #[test]
    fn missing_transport_evidence_remains_unknown_even_with_final_qualified_usage() {
        for missing in [0, 1, 2, 3] {
            let dir = tempfile::tempdir().unwrap();
            let mut ledger =
                SqliteLedger::open(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
            let intent = admit(&mut ledger);
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap();
            let mut evidence = evidence(&ledger, &intent);
            match missing {
                0 => evidence.terminal = None,
                1 => evidence.dispatch_observed_at_ms = None,
                2 => evidence.dropped_observations = 1,
                _ => evidence.invalid = true,
            }
            ledger
                .record_terminal_with_attempt_durable(
                    "team",
                    &intent.execution_id,
                    &usage(),
                    1000,
                    &evidence,
                )
                .unwrap();
            assert!(matches!(
                ledger.settle_terminal_durable("team", &intent.execution_id),
                Err(EvidenceError::UnresolvedObservation)
            ));
            assert_eq!(
                ledger
                    .recovery_page_durable("team", None, 10)
                    .unwrap()
                    .entries[0]
                    .state,
                RecoveryState::UnresolvedObservation
            );
            assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
            assert_eq!(ledger.reserved_durable("upstream:models").unwrap(), 100);
        }
    }

    #[test]
    fn required_attempt_corruption_fails_closed_and_old_rows_do_not_gain_attempts() {
        for corruption in [
            "DELETE FROM budget_physical_attempt",
            "UPDATE budget_physical_attempt SET evidence_json='{}'",
            "UPDATE budget_dispatch_fence SET attempt_id=NULL",
            "UPDATE budget_physical_attempt SET authorized_at=authorized_at+1",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut ledger =
                SqliteLedger::open(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
            let intent = admit(&mut ledger);
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap();
            ledger.conn.execute_batch(corruption).unwrap();
            assert!(
                ledger
                    .physical_attempt_durable("team", &intent.execution_id)
                    .is_err(),
                "{corruption}"
            );
            assert!(ledger.recovery_page_durable("team", None, 10).is_err());
            assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
            assert_eq!(ledger.reserved_durable("upstream:models").unwrap(), 100);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        let intent = admit(&mut ledger);
        ledger
            .conn
            .execute("UPDATE budget_dispatch_fence SET attempt_version=0", [])
            .unwrap();
        ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap();
        ledger
            .conn
            .execute_batch(
                "DROP TABLE budget_physical_attempt;
            ALTER TABLE budget_dispatch_fence DROP COLUMN attempt_id;
            ALTER TABLE budget_dispatch_fence DROP COLUMN attempt_version;",
            )
            .unwrap();
        drop(ledger);
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        assert!(ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .is_none());
        ledger
            .record_terminal_durable("team", &intent.execution_id, &usage(), 1000)
            .unwrap();
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
    }
}
