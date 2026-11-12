//! Single physical-attempt identity and immutable transport evidence. Authorization
//! allocates an identity; only transport observations establish dispatch/termination.
use super::*;
use serde::{Deserialize, Serialize};

const MAX_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportOutcome {
    Success,
    ProviderRejected,
    TransportError,
    Timeout,
    Cancelled,
    IncompleteStream,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportTerminal {
    pub observed_at_ms: i64,
    pub outcome: TransportOutcome,
    pub provider_status: Option<u16>,
    pub provider_request_id: Option<String>,
}

/// Configured diagnostic identity observed at dispatch. These labels never
/// authorize a backend, choose a budget, prove billing or qualify source usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedProvider {
    pub provider: String,
    pub model: Option<String>,
}

impl ObservedProvider {
    pub fn from_labels(provider: String, model: Option<String>) -> Option<Self> {
        let valid = |value: &str, cap: usize| {
            !value.is_empty() && value.len() <= cap && !value.chars().any(char::is_control)
        };
        (valid(&provider, 128) && model.as_ref().is_none_or(|value| valid(value, 512)))
            .then_some(Self { provider, model })
    }
}

/// Transport facts only. Qualified usage is joined through the execution's
/// immutable terminal observation; diagnostic usage is never promoted to authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptEvidence {
    pub version: u8,
    pub attempt_id: String,
    pub attempt_ordinal: u8,
    pub dispatch_observed_at_ms: Option<i64>,
    pub terminal: Option<TransportTerminal>,
    pub dropped_observations: u64,
    pub invalid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_provider: Option<ObservedProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalAttempt {
    pub attempt_id: String,
    pub authorized_at: i64,
    /// Absence after a crash is unknown, never proof of no send.
    pub evidence: Option<AttemptEvidence>,
}

impl AttemptEvidence {
    fn encode(&self) -> Result<String, EvidenceError> {
        validate_execution_id(&self.attempt_id)?;
        let valid_ms = |at: i64| at >= 0 && OffsetDateTime::from_unix_timestamp(at / 1000).is_ok();
        let diagnostic_label = |value: &str, cap: usize| {
            !value.is_empty() && value.len() <= cap && !value.chars().any(char::is_control)
        };
        if !matches!(self.version, 1 | 2)
            || (self.version == 1 && self.observed_provider.is_some())
            || self.observed_provider.as_ref().is_some_and(|observed| {
                !diagnostic_label(&observed.provider, 128)
                    || observed
                        .model
                        .as_ref()
                        .is_some_and(|model| !diagnostic_label(model, 512))
            })
            || self.attempt_ordinal != 1
            || self.dropped_observations > 2
            || self.dispatch_observed_at_ms.is_some_and(|at| !valid_ms(at))
            || self.terminal.as_ref().is_some_and(|t| {
                !valid_ms(t.observed_at_ms)
                    || t.provider_status.is_some_and(|s| !(100..=999).contains(&s))
                    || t.provider_request_id.as_ref().is_some_and(|s| {
                        s.is_empty() || s.len() > 256 || s.chars().any(char::is_control)
                    })
            })
        {
            return Err(EvidenceError::InvalidInput);
        }
        let json = serde_json::to_string(self).map_err(|_| EvidenceError::InvalidInput)?;
        if json.len() > MAX_BYTES {
            return Err(EvidenceError::InvalidInput);
        }
        Ok(json)
    }

    /// Complete transport evidence is necessary but not sufficient for settlement.
    /// Time comparisons use no wall-clock ordering assumption.
    pub fn complete(&self) -> bool {
        self.dispatch_observed_at_ms.is_some()
            && self.terminal.is_some()
            && self.dropped_observations == 0
            && !self.invalid
    }
}

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS budget_physical_attempt (
        execution_id TEXT PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        attempt_id TEXT UNIQUE NOT NULL CHECK(length(CAST(attempt_id AS BLOB))=64),
        authorized_at INTEGER NOT NULL,
        evidence_json TEXT CHECK(length(CAST(evidence_json AS BLOB))<=4096)
    );",
    )?;
    for (name, declaration) in [
        (
            "attempt_version",
            "INTEGER NOT NULL DEFAULT 0 CHECK(attempt_version IN (0,1))",
        ),
        (
            "attempt_id",
            "TEXT CHECK(length(CAST(attempt_id AS BLOB))=64)",
        ),
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('budget_dispatch_fence') WHERE name=?1)",
            [name],
            |r| r.get(0),
        )?;
        if !exists {
            conn.execute(
                &format!("ALTER TABLE budget_dispatch_fence ADD COLUMN {name} {declaration}"),
                [],
            )?;
        }
    }
    Ok(())
}

pub(super) fn allocate(
    conn: &Connection,
    execution_id: &str,
    at: i64,
) -> Result<(), EvidenceError> {
    let required: i64 = conn.query_row(
        "SELECT attempt_version FROM budget_dispatch_fence WHERE execution_id=?1",
        [execution_id],
        |r| r.get(0),
    )?;
    if required == 1 {
        let id = opaque_id()?;
        conn.execute("INSERT INTO budget_physical_attempt(execution_id,attempt_id,authorized_at) VALUES (?1,?2,?3)", params![execution_id,id,at])?;
        conn.execute(
            "UPDATE budget_dispatch_fence SET attempt_id=?2 WHERE execution_id=?1",
            params![execution_id, id],
        )?;
    }
    Ok(())
}

pub(super) fn read(
    conn: &Connection,
    execution_id: &str,
) -> Result<Option<PhysicalAttempt>, EvidenceError> {
    type Row = (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        bool,
    );
    let row: Option<Row> = conn.query_row(
        "SELECT f.attempt_version,f.phase,f.transitioned_at,
         CASE WHEN f.attempt_id IS NULL THEN NULL WHEN typeof(f.attempt_id)='text' AND length(CAST(f.attempt_id AS BLOB))=64 THEN f.attempt_id ELSE '!' END,
         CASE WHEN typeof(a.attempt_id)='text' AND length(CAST(a.attempt_id AS BLOB))=64 THEN a.attempt_id END,
         a.authorized_at,
         CASE WHEN typeof(a.evidence_json)='text' AND length(CAST(a.evidence_json AS BLOB))<=4096 THEN a.evidence_json END,
         a.evidence_json IS NOT NULL
         FROM budget_execution_intent i LEFT JOIN budget_dispatch_fence f USING(execution_id)
         LEFT JOIN budget_physical_attempt a USING(execution_id) WHERE i.execution_id=?1",
        [execution_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)),
    ).optional()?;
    let (id, at, json, has_json) = match row {
        None
        | Some((None | Some(0), _, _, None, None, None, None, false))
        | Some((Some(1), Some(0 | 2), _, None, None, None, None, false)) => return Ok(None),
        Some((
            Some(1),
            Some(1),
            Some(at),
            Some(required),
            Some(id),
            Some(authorized),
            json,
            has_json,
        )) if required == id && at == authorized => (id, at, json, has_json),
        _ => return Err(EvidenceError::CorruptObservation),
    };
    validate_execution_id(&id).map_err(|_| EvidenceError::CorruptObservation)?;
    if OffsetDateTime::from_unix_timestamp(at).is_err() || (has_json && json.is_none()) {
        return Err(EvidenceError::CorruptObservation);
    }
    let evidence = json
        .map(|json| {
            let evidence: AttemptEvidence =
                serde_json::from_str(&json).map_err(|_| EvidenceError::CorruptObservation)?;
            if evidence.attempt_id != id
                || evidence
                    .encode()
                    .map_err(|_| EvidenceError::CorruptObservation)?
                    != json
            {
                return Err(EvidenceError::CorruptObservation);
            }
            Ok(evidence)
        })
        .transpose()?;
    Ok(Some(PhysicalAttempt {
        attempt_id: id,
        authorized_at: at,
        evidence,
    }))
}

pub(super) fn record(
    conn: &Connection,
    execution_id: &str,
    evidence: Option<&AttemptEvidence>,
) -> Result<(), EvidenceError> {
    match (read(conn, execution_id)?, evidence) {
        (None, None) => Ok(()),
        (Some(attempt), Some(evidence)) if attempt.attempt_id == evidence.attempt_id => {
            let json = evidence.encode()?;
            if let Some(original) = attempt.evidence {
                return if original == *evidence {
                    Ok(())
                } else {
                    Err(EvidenceError::ConflictingObservation)
                };
            }
            let changed = conn.execute("UPDATE budget_physical_attempt SET evidence_json=?2 WHERE execution_id=?1 AND evidence_json IS NULL", params![execution_id,json])?;
            if changed != 1 {
                return Err(EvidenceError::InconsistentDispatch);
            }
            Ok(())
        }
        _ => Err(EvidenceError::ConflictingObservation),
    }
}

impl SqliteLedger {
    pub fn physical_attempt_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<PhysicalAttempt>, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let (id, stored_scope, _) =
            intent_binding(&tx, execution_id)?.ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        dispatch::checked_fence(&tx, scope, execution_id, id)?;
        let attempt = read(&tx, execution_id)?;
        tx.commit()?;
        Ok(attempt)
    }
}

#[cfg(test)]
mod diagnostic_version_tests {
    use super::*;
    #[test]
    fn old_v1_stays_byte_identical_and_v2_labels_do_not_change_transport_completeness() {
        let old = format!("{{\"version\":1,\"attempt_id\":\"{}\",\"attempt_ordinal\":1,\"dispatch_observed_at_ms\":1,\"terminal\":{{\"observed_at_ms\":2,\"outcome\":\"success\",\"provider_status\":200,\"provider_request_id\":null}},\"dropped_observations\":0,\"invalid\":false}}","a".repeat(64));
        let mut evidence: AttemptEvidence = serde_json::from_str(&old).unwrap();
        assert_eq!(evidence.encode().unwrap(), old);
        assert!(evidence.complete());
        evidence.version = 2;
        evidence.observed_provider = Some(ObservedProvider {
            provider: "configured-vendor".into(),
            model: Some("physical-model".into()),
        });
        let encoded = evidence.encode().unwrap();
        let reread: AttemptEvidence = serde_json::from_str(&encoded).unwrap();
        assert_eq!(reread, evidence);
        assert!(evidence.complete());
        evidence.observed_provider =
            ObservedProvider::from_labels(String::new(), Some("model".into()));
        assert!(evidence.observed_provider.is_none());
        assert!(evidence.encode().is_ok());
        assert!(evidence.complete());
        evidence.terminal = None;
        assert!(!evidence.complete());
        evidence.version = 1;
        evidence.observed_provider = Some(ObservedProvider {
            provider: "vendor".into(),
            model: None,
        });
        assert!(evidence.encode().is_err());
    }
}
